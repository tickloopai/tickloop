use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{IsTerminal, Write},
    path::PathBuf,
};

use tick_connect::{access::TickAccess, herdr::HerdrContext};

use tickloop_agent::{
    model::{ModelConfig, ReasoningEffort, Usage, cards::ProviderPatch, model_client},
    permissions::{Decision, ExecutionMode, PermissionRule, PermissionScope},
    runtime::{
        AgentError, AgentHostOptions, BranchOutcome, BranchRequest, ChatSnapshot,
        DirectoryTrustPolicy, ENTER_PLAN_MODE_TOOL_NAME, EventKind, InputContext, LoopId,
        PermissionChange, PromptPolicy, QuestionRequest, Receipt, RequestKind, RequestOutcome,
        ShellTaskNotice, ShellTaskPromptWatchdog, ShellTaskSnapshot, ShellTaskState,
        ShellTaskSubscriptionEvent, TaskNotification, TaskNotificationStatus, TodoStatus, Trigger,
        TriggerSource, UserShellRecord, UserShellState,
    },
    settings::{LoadedSettings, UserSettings},
};
use tokio::sync::{broadcast, mpsc};

mod clipboard;
mod clipboard_hint;
mod completion;
mod console;
mod conversation;
mod conversation_picker;
pub mod diff_catalog;
pub mod diff_dialog;
pub mod diff_view;
mod directed_summary;
mod display;
mod editor;
mod execution;
mod external_editor;
mod file_mentions;
mod font_manager;
mod footer;
mod hyperlink;
mod image_cache;
pub(crate) use image_cache::IMAGE_CACHE_DIRECTORY;
mod input_history;
pub(crate) use input_history::{PASTE_CACHE_DIRECTORY, PASTE_FILE_EXTENSION};
#[cfg(test)]
mod child_config_tests;
mod denials_surface;
mod input_view;
#[cfg(test)]
mod late_receipt_tests;
mod login;
mod loops;
mod menus;
#[cfg(test)]
mod navigation_tests;
mod new_agent_draft;
#[cfg(test)]
mod owner_loss_tests;
mod paste_blocks;
mod pasted_image_paths;
mod permissions_surface;
mod presentation;
mod process_registry;
mod questions;
#[cfg(test)]
mod recovery_tests;
mod renderer;
mod reply_lines;
mod running_shimmer;
mod scrollback;
mod selection;
mod settings;
mod shell_input;
mod status_area;
mod style;
mod subscription_surface;
mod task_access;
mod task_surface;
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_support;
mod thinking_preview;
mod transcript;
mod workflows;

use console::{Console, InputEvent, RendererSwitch};
use diff_catalog::ChangeCatalog;
use directed_summary::{DirectedSummaryTasks, SummaryTaskCompletion};
use display::{DisplayProfile, Glyph};
use execution::{
    ConsoleExecution as Agent, CreateAcknowledgement, CreationAcceptance, DraftCreationError,
    DraftPermissionsError, InputResolution, RuleSave,
};
use footer::FooterItemId;
use login::{LoginFlow, LoginUpdate, LogoutFlow};
use menus::{MenuOutcome, Menus};
use new_agent_draft::ConfigTarget;
use permissions_surface::{PermissionsEffect, PermissionsReturn, PermissionsSubject};
use presentation::{MessageGroups, ToolPresentation};
use process_registry::ProcessRegistration;
use renderer::Renderer;
use reply_lines::ReplyLines;
use running_shimmer::RunningShimmer;
use scrollback::InputEcho;
use status_area::StatusItem;
use style::Role;

const DISCONNECTED_CACHED_VIEW_NOTICE: &str = "Disconnected · cached view";
const VIEW_REGISTRATION_UNAVAILABLE_NOTICE: &str = "View registration unavailable";
use subscription_surface::SubscriptionEffect;
use tickloop_agent::agent_access::{
    AgentDetailQuery, ConfigurationChange, ObservationRequest, ObservationValue,
};

use task_access::{ShellTaskCenter, ShellTaskSubscription};
/// How often an open subscription view re-reads the Agent's snapshot; an unchanged one repaints
/// nothing, so the poll only has to be fast enough to look live.
use task_surface::{TaskOutputUpdate, TaskSurfaceEffect};
use thinking_preview::ThinkingPreview;
use workflows::Workflow;

const SUBSCRIPTION_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

macro_rules! out {
    ($console:expr, $($argument:tt)*) => { $console.write(&format!($($argument)*))? };
}
macro_rules! outln {
    ($console:expr $(,)?) => { $console.write("\n")? };
    ($console:expr, $($argument:tt)*) => { $console.write_line(&format!($($argument)*))? };
}

const COMMAND_OUTPUT_BYTES: usize = 16 * 1024;
const ANIMATION_FRAME_DURATION: std::time::Duration = std::time::Duration::from_millis(32);
/// The activity shown while a run works without an in-progress todo.
const WORKING_ACTIVITY: &str = "Working";

struct RunningActivity {
    chat_id: uuid::Uuid,
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
        let reduced_motion = console.display_preferences()?.prefers_reduced_motion;
        if let Some(preview) = preview {
            return Ok(preview.spans(console.conversation_width()?, reduced_motion));
        }
        let mut row = style::RoleLine::default();
        row.push("• ", Some(Role::MutedText));
        shimmer.append(&mut row, &self.label, reduced_motion);
        let mut details = self
            .started_at
            .map(|started| format_activity_elapsed(started.elapsed()))
            .into_iter()
            .collect::<Vec<_>>();
        if self.interruptible {
            details.push("Esc to interrupt".into());
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
    chat: uuid::Uuid,
    pending: &HashMap<uuid::Uuid, Receipt>,
    thinking_previews: &mut HashMap<uuid::Uuid, ThinkingPreview>,
    reply_lines: &mut HashMap<uuid::Uuid, ReplyLines>,
    unpaced_runs: &mut HashSet<uuid::Uuid>,
) -> Result<(), String> {
    let _frame_hold = console.hold_frames()?;
    for (run_id, receipt) in pending {
        if receipt.chat_id != chat {
            continue;
        }
        unpaced_runs.insert(*run_id);
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
    let console = console.for_message(reply.message_id.clone());
    if starts_message {
        console.write_roles(&[("\n".into(), None)])?;
    }
    console.write_markdown(&line, starts_message)?;
    Ok(())
}

fn write_completion_notice(console: &Console, usage: &Usage) -> Result<(), String> {
    let display = console.display_profile()?;
    let interactive = console.is_interactive();
    console.write_completion_notice(&[
        ((if interactive { "\n\n" } else { "\n" }).into(), None),
        (
            if interactive {
                format!("{} Done", display.glyph(Glyph::Success))
            } else {
                "Reply completed".into()
            },
            Some(Role::Success),
        ),
        (usage_text(usage), Some(Role::MutedText)),
        ("\n".into(), None),
    ])
}

fn ends_thinking(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Text(_)
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
    if name == "/loops" && command.split_whitespace().nth(1) == Some("schedule") {
        return CommandTiming::AfterTurn;
    }
    if is_rewind_command(name)
        || matches!(
            name,
            "/exit"
                | "/help"
                | "/details"
                | "/diff"
                | "/status"
                | "/artifacts"
                | "/settings"
                | "/permissions"
                | "/mode"
                | "/model"
                | "/provider"
                | "/thinking"
                | "/loops"
                | "/tasks"
                | "/subscriptions"
                | "/operations"
                | "/rules"
                | "/plan"
                | "/interrupt"
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

fn is_task_surface_command(command: &str) -> bool {
    command == "/tasks"
}

fn is_subscription_view_command(command: &str) -> bool {
    command == "/subscriptions"
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

fn switch_to_saved_branch(console: &Console, child_chat_id: uuid::Uuid) -> Result<(), String> {
    match console.commit_branch_and_reset_chat(child_chat_id) {
        Ok(()) => Ok(()),
        Err(reset_error) => {
            let recovery = format!(
                "Branch {child_chat_id} was saved, but the terminal could not switch to it: {}. Use /resume to find it or /resume {child_chat_id} to open it.",
                safe_line(&reset_error)
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
    source_chat_id: Option<uuid::Uuid>,
    source_loop_id: Option<uuid::Uuid>,
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
        return Err(branch_failure_message(&AgentError::Busy));
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
            request_id: uuid::Uuid::new_v4(),
            source_chat_id,
            source_loop_id,
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
        && !is_task_surface_command(command)
        && !is_subscription_view_command(command)
        && !is_rewind_command(command)
        && !is_branch_command(command)
}

fn parse_execution_mode(value: &str) -> Result<(ExecutionMode, Option<&'static str>), String> {
    match value.trim() {
        "ask" | "acceptEdits" | "accept-edits" => Ok((ExecutionMode::AcceptEdits, None)),
        "auto" => Ok((ExecutionMode::Auto, None)),
        "fullAccess" | "full-access" => Ok((ExecutionMode::FullAccess, None)),
        "plan" => Ok((ExecutionMode::Plan, None)),
        "manual" => Ok((
            ExecutionMode::AcceptEdits,
            Some("`manual` was replaced by Ask; use `ask`"),
        )),
        "default" => Ok((
            ExecutionMode::AcceptEdits,
            Some("`default` is deprecated and maps to Ask; use `ask`"),
        )),
        "dontAsk" => Err("`dontAsk` was replaced by the --deny-on-prompt launch policy".into()),
        "bypass" => Err(
            "`bypass` was replaced by --dangerously-skip-permissions and requires external isolation"
                .into(),
        ),
        _ => Err("choose auto, ask, fullAccess or plan".into()),
    }
}

fn ensure_startup_directory_trust(
    settings: Option<&UserSettings>,
    workdir: &std::path::Path,
    interactive: bool,
) -> Result<(), String> {
    let settings = settings.ok_or_else(|| {
        "cannot record working-directory trust because the user settings directory is unavailable; set TICKLOOP_CONFIG_DIR"
            .to_owned()
    })?;
    if settings.is_working_directory_trusted(workdir) {
        return Ok(());
    }
    if !interactive {
        return Err(format!(
            "working directory {} is not trusted; start tickloop interactively in that directory once to review and trust it",
            safe_text(&workdir.to_string_lossy())
        ));
    }
    eprintln!(
        "Trust working directory {}? Project content may influence Agent behavior. [y/N]",
        safe_text(&workdir.to_string_lossy())
    );
    std::io::stderr()
        .flush()
        .map_err(|error| format!("cannot display directory trust request: {error}"))?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|error| format!("cannot read directory trust decision: {error}"))?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Err(
            "working directory was not trusted; no Chat or model request was started".into(),
        );
    }
    settings
        .trust_working_directory(workdir)
        .map_err(|error| format!("cannot save working-directory trust: {error}"))
}

pub fn print_help() {
    println!("{}", help_text());
}

fn help_text() -> String {
    let guidance = r#"Tick Loop Agent
Usage: tickloop [--config FILE] [--cwd DIRECTORY]
       [--mode acceptEdits|auto|plan] [--deny-on-prompt | --dangerously-skip-permissions]
       [--working-directory-is-trusted]
       [-c | --continue | -r [CHAT] | --resume [CHAT] | --chat-id CHAT] [--fork-session]
       [--no-session-persistence]
       tickloop service stop

service stop ends the local Agent service and its running Agents; saved history is kept.
Run tickloop again to start this build. TICKLOOP_AGENT_RUNTIME_DIR selects the service.

The working directory must exist; omitting --cwd uses the current directory.
Unknown directories require interactive trust. --working-directory-is-trusted is an explicit host assertion for externally controlled automation and does not save trust.
Starting without -c or -r opens a new Chat.
-c, --continue resumes the most recent Chat in this directory.
-r, --resume [CHAT] resumes the given Chat, or opens the Chat picker when CHAT is omitted.
--fork-session copies the resumed conversation into a new Chat; use it with -c or -r.
--chat-id opens or creates one exact saved Chat identity.
Chats are saved under ~/.tickloop, or TICKLOOP_CONFIG_DIR when it is set.
--no-session-persistence runs without saving Chats; so does cleanupPeriodDays 0 in settings.json.
Transcripts, large tool results, plans, file backups and paste cache older than cleanupPeriodDays (default 30) are removed in the background 10 minutes after the first prompt (at start when input is not a terminal), later while you are typing.
/provider opens Codex subscription authorization; choose browser or device code there. Judgment settings manages the separate Jev (TypeSafe) key.
/provider: press s to sign out of Codex locally; requests already sent are not revoked.
/model choices and persistent rules are saved in settings.json there; --config takes precedence over the saved model.

Enter a task; follow-up inputs are accepted while the current run continues.
Queued messages, commands and Shell input appear above the editor; Up on the first visual row restores editable queued input.
Down on the last visual input row, after history ends, selects Footer Tasks/Todo without submitting the draft.
Footer Tasks/Todo starts folded: click a label or press Enter/Space to expand; Space folds, Esc returns to the editor.
Footer: h/l or arrows choose; Enter on an expanded Task opens details; other typed keys return to the editor.
Shift+Up/Down opens background Tasks directly when any are running.
/loops opens Loop management; /loops new starts another goal in this Chat; /loops UUID selects one.
/loops trigger [GOAL_ID] reads its trigger.
/loops schedule GOAL_ID SECONDS [--for DURATION_SECONDS] PROMPT sets a local timer and optional deadline.
/clear starts a new Chat and stops this Chat's Shell tasks that are not in the background.
/branch [NAME] copies this conversation at its current point and switches to the child Chat.
/resume opens the Chat picker; /resume CHAT switches to that Chat without running it.
Chats you switch away from keep running their goals in this process; /tasks lists Shell tasks from every Chat.
/tickloop [REQUEST] loads the system Workspace Skill; other discovered Skills also use /NAME [ARGUMENTS].
/provider configures connections, sign-in and the separate Jev (TypeSafe) key without changing the Chat model.
/model selects models from configured Providers; p opens Provider configuration. /model FILE loads an advanced Chat configuration. Model choices also apply to new Chats.
/thinking opens the Thinking picker; /thinking LEVEL sets a supported level. Auto uses the model default; /settings toggles Thinking.
/settings opens settings for this Chat; /permissions shows Auto, Ask and Full Access and a separate recent-activity entry.
/permissions → Auto opens Auto settings, where you can select LLM or Judgment, manage review rules and Environment, and open the shared Jev (TypeSafe) key settings.
Explicit Ask rules always require a human once-only answer. Auto defaults to this Chat's model; Jev technical failure falls back to it without changing the saved choice.
/settings → Theme opens the theme picker; the choice applies to this terminal session only.
Fullscreen draws on the alternate screen; start with TICKLOOP_AGENT_TUI=inline where that is unusable, such as iTerm2 tmux -CC.
Fullscreen reads the wheel to scroll the conversation; TICKLOOP_AGENT_DISABLE_MOUSE=1 hands the wheel back to the terminal and restores click-and-drag selection.
/diff opens current working-tree changes and Agent file changes saved in this process without calling the model.
Typing / or a command prefix shows completion; Tab fills the first match.
↑/↓ or Ctrl+N/P selects a completion; Enter fills a selected match or sends the draft.
Typing @ and part of a local path offers files and directories; only an accepted candidate is attached.
Focused lists also accept j/k and Ctrl+N/P; Enter confirms; Esc cancels or denies.
Input uses normal editing with Emacs keys: Ctrl+A/E line start/end, Ctrl+B/F and Alt+B/F move by character and word.
Ctrl+H deletes backward; Ctrl+K/U/W and Alt+D/Alt+Backspace kill; Ctrl+Y yanks and Alt+Y cycles; Ctrl+_ undoes; Ctrl+S stashes the draft.
Ctrl+G edits the draft in $VISUAL or $EDITOR (which may be Vim); saving and quitting updates the draft.
Ctrl+V attaches the clipboard image as [Image #N], moved and deleted as one block; the model must accept image input.
Ctrl+R opens history search.
History search uses Ctrl+S for session/project/all scopes; Enter/Tab fills the draft.
Ctrl+J or modified Enter inserts a real newline; Ctrl+C clears a draft before interrupting.
With an empty prompt, Ctrl+C or Ctrl+D must be pressed twice within 800ms to exit.
With a draft, Ctrl+D deletes forward instead of exiting.
On Unix, Ctrl+Z suspends this Agent process; run fg in the shell to resume it.
Esc twice clears a draft into history, or opens the rewind review when input is empty.
/rewind opens the Rewind checkpoint selector.
Ctrl+O or /details opens the full conversation transcript; Ctrl+O, Esc or q returns.
In transcript: / searches, Enter accepts, n/N moves between matches; Esc cancels search.
j/k or arrows scroll; g/G or Home/End jumps; {/} moves between user prompts.
Persistent rules apply to every Chat in this working directory; session rules end with this process.
/cwd [PATH] shows or sets the main directory.
/plan enters Plan mode and opens a saved plan for the selected goal; /plan GOAL views a specific saved plan.
/tasks lists Shell processes.
/subscriptions lists this Chat's Tick Subscriptions; Enter changes a cadence, d cancels one.
Shell task state and per-operation output are available only in this process; they are gone after a restart.
Background Shell completions are added to their Chat as task notifications.
A completion continues the goal that started the command; commands without a goal only record the notification.
/compact [INSTRUCTIONS] summarizes context behind a compaction boundary.
/help advanced lists advanced commands; /exit closes this View; Agent work continues."#;
    format!(
        "{guidance}\n\nCommands and Skills:\n{}",
        completion::command_help(false)
    )
}

pub async fn run() -> Result<(), String> {
    run_with_arguments(std::env::args().skip(1)).await
}

async fn run_with_arguments(arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let mut arguments = arguments.peekable();
    let mut config_path = None;
    let mut execution_mode_override = None;
    let mut prompt_policy = PromptPolicy::Interactive;
    let mut deny_on_prompt = false;
    let mut unrestricted = false;
    let mut caller_managed_trust = false;
    let mut continue_recent = false;
    let mut resume: Option<Option<uuid::Uuid>> = None;
    let mut exact_chat = None;
    let mut fork_session = false;
    let mut session_persistence = true;
    let mut workdir = std::env::current_dir()
        .map_err(|_| "cannot read the current directory; use an accessible directory".to_owned())?;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            "--config" => {
                config_path = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or("--config requires a YAML or JSON file path")?,
                ))
            }
            "--auto-reviewer-config" | "--permission-checker-config" => {
                return Err("Independent Auto reviewer configuration has moved to /permissions → Auto mode. Remove this startup flag and choose LLM or Judgment there.".into());
            }
            "--mode" => {
                let value = arguments
                    .next()
                    .ok_or("--mode requires acceptEdits, auto or plan")?;
                let (mode, warning) = parse_execution_mode(&value)?;
                if let Some(warning) = warning {
                    eprintln!("Warning: {warning}");
                }
                execution_mode_override = Some(mode);
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
            "--cwd" => {
                workdir = PathBuf::from(arguments.next().ok_or("--cwd requires a directory path")?)
            }
            "-c" | "--continue" => continue_recent = true,
            "-r" | "--resume" => {
                let chat = arguments
                    .next_if(|value| !value.starts_with('-'))
                    .map(|value| {
                        value.parse::<uuid::Uuid>().map_err(|_| {
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
                exact_chat = Some(value.parse::<uuid::Uuid>().map_err(|_| {
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
    let resumes_saved_chat = continue_recent || resume.is_some();
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
    if !session_persistence && (resumes_saved_chat || exact_chat.is_some()) {
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
    if unrestricted {
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
    let interactive_terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let config_root = config_root_directory().map(normalize_state_root);
    // 与 `Agent::open` 一样使用规范化的根目录，启动时与之后新建 Chat 时报告的设置警告一致。
    let user_settings = config_root
        .as_deref()
        .map(|root| UserSettings::in_root(&root.canonicalize().unwrap_or_else(|_| root.into())));
    if !caller_managed_trust {
        ensure_startup_directory_trust(user_settings.as_ref(), &workdir, interactive_terminal)?;
    }
    // 照 Claude Code：启动参数指定的模型优先，其次是 `/model` 保存在用户设置中的选择。
    let explicit_config = config_path
        .as_deref()
        .map(|path| read_config_with_provider(path, user_settings.as_ref()))
        .transpose()?;
    let mut saved_settings = user_settings.as_ref().map(UserSettings::load);
    let saved_model = if explicit_config.is_some() {
        if let Some(settings) = saved_settings.as_mut() {
            let _ = settings.model();
        }
        None
    } else {
        saved_settings
            .as_mut()
            .map(LoadedSettings::configured_model)
            .transpose()
            .map_err(|error| {
                if let Some(root) = config_root.as_ref()
                    && root.exists()
                    && !root.is_dir()
                {
                    format!(
                        "cannot use Agent state root {}: {error}; set TICKLOOP_CONFIG_DIR to a readable directory. --no-session-persistence disables Chat storage but still reads model settings",
                        safe_text(&root.to_string_lossy())
                    )
                } else {
                    format!("invalid model settings: {error}")
                }
            })?
            .flatten()
    };
    let display_preferences = saved_settings
        .as_mut()
        .map(LoadedSettings::display_preferences)
        .unwrap_or_default();
    let status_lines = saved_settings
        .as_mut()
        .and_then(LoadedSettings::status_lines);
    let startup_settings_warnings = saved_settings
        .map(LoadedSettings::into_warnings)
        .unwrap_or_default();
    let explicit_provider = explicit_config
        .as_ref()
        .and_then(|(_, provider)| provider.clone());
    let config = if let Some((config, _)) = explicit_config {
        config
    } else if let Some(model) = saved_model {
        model
    } else {
        crate::model::cards::ModelCatalog::defaults()
            .get(
                "deepseek-api",
                tickloop_agent::model::access::AccessCatalog::defaults()
                    .recommended_model("deepseek-api")
                    .expect("built-in DeepSeek recommendation"),
            )
            .expect("bundled default model exists")
    };
    if let (Some(settings), Some(provider)) = (&user_settings, &explicit_provider) {
        settings.set_transient_provider(provider.clone());
    }
    let host_options = AgentHostOptions {
        execution_mode_override,
        prompt_policy,
        directory_trust: if caller_managed_trust {
            DirectoryTrustPolicy::CallerManaged
        } else {
            DirectoryTrustPolicy::RequireUserTrust
        },
    };
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
    let (host_work_context, view_host) = if let Some(context) = HerdrContext::from_environment()? {
        let access = TickAccess::new(context.clone());
        let pane = access
            .caller()
            .await
            .map_err(|error| format!("cannot inspect the calling Herdr Pane: {error}"))?;
        let metadata = std::fs::metadata(&context.socket)
            .map_err(|error| format!("cannot inspect Herdr endpoint: {error}"))?;
        let host_instance = format!("herdr:{}:{}", metadata.dev(), metadata.ino());
        (
            Some(crate::agent_access::HostWorkContext {
                socket: context.socket.clone(),
                workspace: Some(pane.workspace_id.clone()),
            }),
            Some(crate::agent_access::HostLocation {
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
    let launch = crate::agent_access::LaunchSpec {
        workdir: workdir.clone(),
        config_root: config_root.clone(),
        model: Some(config.clone()),
        transient_provider: explicit_provider.clone(),
        host_options,
        session_persistence,
        exact_chat: None,
        host_work_context,
        environment: std::env::vars_os()
            .map(|(name, value)| crate::agent_access::EnvironmentValue {
                name: name.as_os_str().as_bytes().to_vec(),
                value: value.as_os_str().as_bytes().to_vec(),
            })
            .collect(),
    };
    #[cfg(test)]
    let agent = if std::env::var_os("TICKLOOP_TEST_CODEX_OAUTH").is_some() {
        let core = crate::agent_daemon::build_runtime(&launch).await?;
        Agent::from_embedded_launch(core, launch)
    } else {
        let client = crate::agent_daemon::connect_or_start().await?;
        Agent::connected(client, launch, view_host)
    };
    #[cfg(not(test))]
    let agent = {
        let client = crate::agent_daemon::connect_or_start().await?;
        Agent::connected(client, launch, view_host)
    };
    if (resumes_saved_chat || exact_chat.is_some()) && !agent.saves_chats() {
        return Err(
            "conversations are not saved because cleanupPeriodDays is 0 in settings.json; set it to a positive number of days to keep and resume Chats".into(),
        );
    }
    let startup = if let Some(chat) = exact_chat {
        let (snapshot, has_history) = match agent.resume_chat(chat).await {
            Ok(snapshot) => (snapshot, true),
            Err(AgentError::NotFound) => {
                agent
                    .restore_empty_chat(chat, workdir.clone())
                    .await
                    .map_err(|error| format!("Failed to create Chat {chat}: {error}"))?;
                (
                    agent
                        .snapshot(chat)
                        .await
                        .map_err(|error| format!("Failed to open Chat {chat}: {error}"))?,
                    false,
                )
            }
            Err(error) => return Err(format!("Failed to resume Chat {chat}: {error}")),
        };
        StartupChat::Resumed(Box::new(snapshot), has_history)
    } else if continue_recent {
        let recent = agent
            .saved_chats(&workdir)
            .await
            .map_err(|error| safe_text(&error.to_string()))?
            .into_iter()
            .next()
            .ok_or("No conversation found to continue")?;
        StartupChat::Resumed(
            Box::new(open_saved_chat(&agent, recent.id, fork_session).await?),
            true,
        )
    } else if let Some(requested) = resume {
        match requested {
            Some(chat) => StartupChat::Resumed(
                Box::new(open_saved_chat(&agent, chat, fork_session).await?),
                true,
            ),
            None => StartupChat::Picker {
                as_new_chat: fork_session,
            },
        }
    } else {
        StartupChat::New
    };
    let (console, input) = Console::start_with_display(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        display_preferences,
    )?;
    let console = &console;
    console.set_status_lines(status_lines)?;
    console.configure_input_history(config_root.as_deref(), &workdir)?;
    // The image cache expires with Transcripts, so a session that saves no Chats caches nothing.
    let image_cache_root = config_root
        .as_ref()
        .filter(|_| agent.saves_chats())
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()));
    console.configure_image_cache(image_cache_root.as_deref())?;
    console.set_input_context(None, &workdir, &[])?;
    initialize_startup_console(
        console,
        &config,
        &workdir,
        execution_mode_override
            .or_else(|| {
                user_settings
                    .as_ref()
                    .and_then(UserSettings::default_execution_mode)
            })
            .unwrap_or_default(),
        startup_settings_warnings,
    )?;
    let result = interact(
        &agent,
        workdir,
        console,
        input,
        StartupState {
            chat: startup,
            explicit_provider,
            user_settings,
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

/// 启动时进入的 Chat：不带参数时新建；`-c`／`-r CHAT` 已在启动前恢复；`-r` 打开选择器。
enum StartupChat {
    New,
    Resumed(Box<tickloop_agent::runtime::ChatSnapshot>, bool),
    Picker { as_new_chat: bool },
}

struct StartupState {
    chat: StartupChat,
    explicit_provider: Option<ProviderPatch>,
    user_settings: Option<UserSettings>,
}

fn initialize_startup_console(
    console: &Console,
    config: &ModelConfig,
    workdir: &std::path::Path,
    execution_mode: ExecutionMode,
    warnings: Vec<String>,
) -> Result<(), String> {
    console.update_status_area(
        execution_mode.to_string(),
        safe_text(&config.model),
        None,
        Vec::new(),
        Vec::new(),
    )?;
    write_banner(console, config, workdir)?;
    show_settings_warnings(console, warnings)?;
    console.set_prompt_enabled(true)
}

/// 按 `--fork-session` 恢复或复制已保存的 Chat，错误文案照 Claude Code。
async fn open_saved_chat(
    agent: &Agent,
    chat: uuid::Uuid,
    as_new_chat: bool,
) -> Result<tickloop_agent::runtime::ChatSnapshot, String> {
    let opened = if as_new_chat {
        agent.resume_as_new_chat(chat).await
    } else {
        agent.resume_chat(chat).await
    };
    opened.map_err(|error| match error {
        AgentError::NotFound => format!("No conversation found with Chat ID: {chat}"),
        error => format!(
            "Failed to resume Chat {chat}: {}",
            safe_text(&error.to_string())
        ),
    })
}

/// Startup banner following Claude Code's condensed logo: the product name in
/// bold with its version dimmed, then model and working directory dimmed. The
/// service endpoint lives in Session status instead, and command tips belong
/// to the footer, as neither appears in Claude Code's banner.
fn write_banner(
    console: &Console,
    config: &ModelConfig,
    workdir: &std::path::Path,
) -> Result<(), String> {
    if !console.is_interactive() {
        outln!(
            console,
            "Tick Loop Agent · {} · {} · {:?}",
            safe_text(&config.name),
            safe_text(&config.model),
            config.protocol
        );
        outln!(
            console,
            "Service: {}\n/loops new starts a new goal; /help lists commands; /exit closes this View; Agent work continues.",
            safe_text(&config.base_url)
        );
        return Ok(());
    }
    console.write_roles(&startup_banner_spans(config, workdir))
}

fn startup_banner_spans(
    config: &ModelConfig,
    workdir: &std::path::Path,
) -> Vec<(String, Option<Role>)> {
    let directory = workdir.to_string_lossy().into_owned();
    let directory = match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && directory.starts_with(&home) => {
            format!("~{}", &directory[home.len()..])
        }
        _ => directory,
    };
    vec![
        ("Tick Loop Agent".into(), Some(Role::Title)),
        (
            format!(" v{}\n", env!("CARGO_PKG_VERSION")),
            Some(Role::MutedText),
        ),
        (
            format!("{} · {:?}\n", safe_text(&config.model), config.protocol),
            Some(Role::MutedText),
        ),
        (
            format!("{}\n", safe_text(&directory)),
            Some(Role::MutedText),
        ),
    ]
}

async fn interact(
    agent: &Agent,
    workdir: PathBuf,
    console: &Console,
    mut input: mpsc::Receiver<InputEvent>,
    startup: StartupState,
    config_root: Option<PathBuf>,
) -> Result<(), String> {
    let user_settings = startup.user_settings;
    agent.use_user_settings(user_settings.clone());
    let mut events = agent.subscribe();
    let mut user_shell_events = agent.subscribe_user_shells();
    let mut task_notifications = agent.subscribe_task_notifications();
    let interactive = console.is_interactive();
    let mut menus = Menus::new(user_settings.clone(), config_root.clone());
    if user_settings.is_none()
        && let Some(provider) = startup.explicit_provider
    {
        menus.add_transient_provider(provider);
    }
    let mut login: Option<LoginFlow> = None;
    let mut login_from_provider = false;
    let mut provider_login_succeeded = false;
    let mut reauthenticate_after_logout = false;
    let mut logout_target = login::SignInTarget::Codex;
    let mut logout: Option<LogoutFlow> = None;
    let mut tools = ToolPresentation::new(!interactive);
    let mut transcript_dirty = false;
    let mut message_groups = MessageGroups::default();
    let mut subscription_refresh = tokio::time::interval(SUBSCRIPTION_REFRESH_INTERVAL);
    subscription_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut transcript_refresh = tokio::time::interval(std::time::Duration::from_millis(100));
    transcript_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut animation_refresh = tokio::time::interval(ANIMATION_FRAME_DURATION);
    animation_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut elapsed_refresh = tokio::time::interval(std::time::Duration::from_secs(1));
    elapsed_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut working_shimmer = RunningShimmer::default();
    let task_center = ShellTaskCenter::new(agent.clone());
    let mut task_subscription = None;
    let mut prompt_watchdog_refresh = tokio::time::interval(std::time::Duration::from_secs(1));
    prompt_watchdog_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut prompt_watchdog = ShellTaskPromptWatchdog::default();
    let prompt_watchdog_epoch = std::time::Instant::now();
    let mut pending: HashMap<uuid::Uuid, Receipt> = HashMap::new();
    let mut thinking_runs = HashSet::new();
    let mut thinking_previews: HashMap<uuid::Uuid, ThinkingPreview> = HashMap::new();
    let mut reply_lines: HashMap<uuid::Uuid, ReplyLines> = HashMap::new();
    let mut unpaced_runs: HashSet<uuid::Uuid> = HashSet::new();
    let mut deferred_events = VecDeque::new();
    let mut merged_echoes = MergedEchoes::default();
    let mut merged_echoes_view = None;
    let mut permissions = HashMap::new();
    let mut questions: HashMap<uuid::Uuid, QuestionRequest> = HashMap::new();
    let mut compactions = tokio::task::JoinSet::new();
    let mut compaction_chats = HashMap::new();
    let mut directed_summaries = DirectedSummaryTasks::default();
    let mut summary_recovery_required = HashSet::new();
    let mut goal = None;
    let mut selected_loops = HashMap::new();
    let mut selected_chat = None;
    let mut global = crate::agent_console::AgentConsole::new(
        agent.access(),
        agent.owner_epoch(),
        workdir.clone(),
        agent.host_location(),
    );
    let mut overview_active = false;
    let mut history_return_overview = false;
    let mut visible_family = Vec::new();
    // The owner fact behind the shared catalog; set by a failed refresh and cleared by the next success.
    let mut owner_unreachable: Option<String> = None;
    let mut global_refresh = tokio::time::interval(std::time::Duration::from_secs(1));
    global_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ui_selected = selected_chat;
    let mut background_notice: Option<(uuid::Uuid, BackgroundRunNotice)> = None;
    let mut transcript_write_error = None;
    let mut loop_trigger_notices: Vec<(uuid::Uuid, String)> = Vec::new();
    let mut displayed_sequences: HashMap<uuid::Uuid, u64> = HashMap::new();
    let mut displayed_assistant_messages = HashSet::new();
    let mut eof = false;
    let mut failed = false;
    let mut status_dirty = true;
    let mut activity_dirty = false;
    let mut running_activity: Option<RunningActivity> = None;
    // A replayed or restored run has no known start; only a live Started event begins this clock.
    let mut observed_run_starts = HashMap::new();
    let mut cached_activity_status: Option<(uuid::Uuid, String)> = None;
    let mut menu_outcome: Option<MenuOutcome> = None;
    let mut draft_question = None;
    let mut menu_commands = VecDeque::new();
    let mut paused_inputs = VecDeque::new();
    let mut foreground_shells = HashMap::new();
    let mut user_shell_offsets = HashMap::new();
    let mut user_shell_chats = HashMap::new();
    let mut deferred_shell_updates: HashMap<uuid::Uuid, UserShellRecord> = HashMap::new();
    let mut backgrounded_shells = HashSet::new();
    match startup.chat {
        StartupChat::New => {}
        StartupChat::Resumed(snapshot, has_history) => {
            goal = show_opened_chat(console, agent, &snapshot, has_history)?;
            selected_chat = Some(snapshot.id);
            displayed_sequences.insert(snapshot.id, snapshot.last_sequence);
            tools.restore(&snapshot.conversation_events());
            sync_snapshot(&snapshot, &mut pending, &mut permissions, &mut questions);
        }
        StartupChat::Picker { as_new_chat } => {
            menus.conversations(
                console,
                Workflow::chats(agent, &workdir, None, as_new_chat).await?,
            )?;
        }
    }
    if let Some(chat) = selected_chat {
        let snapshot = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        menus.warm_model_directory(snapshot.model);
    } else if let Some(model) = agent.launch_spec().model {
        menus.warm_model_directory(model);
    }
    agent
        .display_chat(selected_chat)
        .await
        .map_err(|error| error.to_string())?;
    let mut registration = config_root.as_deref().and_then(|root| {
        let registration =
            ProcessRegistration::register(root, selected_chat, &workdir, interactive);
        process_registry::count_concurrent_processes(root);
        registration
    });
    loop {
        observed_run_starts.retain(|run, _| pending.contains_key(run));
        if let Some(chat) = selected_chat {
            selected_loops.insert(chat, goal);
        }
        menus.set_new_agent_directory(global.projection().dispatch_directory);
        if !overview_active && ui_selected != selected_chat {
            agent
                .display_chat(selected_chat)
                .await
                .map_err(|error| error.to_string())?;
            let visible = match selected_chat {
                Some(chat) => Some(
                    agent
                        .reference(chat)
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
            && registration.chat_id() != selected_chat
        {
            registration.update_chat(selected_chat);
        }
        if login.is_none() && logout.is_none() && login_from_provider {
            login_from_provider = false;
            if let Some(chat) = selected_chat {
                agent
                    .refresh_configuration(chat)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            menus
                .resume_provider_login(agent, console, provider_login_succeeded)
                .await?;
        }
        if let Some(outcome) = menu_outcome.take() {
            if history_return_overview {
                if outcome.selection.is_some() {
                    history_return_overview = false;
                } else if menus.id().is_none() {
                    global
                        .interpret(crate::agent_console::Intent::Overview)
                        .map_err(|error| error.to_string())?;
                    if let Err(error) = global.refresh().await {
                        owner_unreachable = Some(format!(
                            "{DISCONNECTED_CACHED_VIEW_NOTICE} · {}",
                            safe_text(&error.to_string())
                        ));
                        console.set_connection_notice(owner_unreachable.clone())?;
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
                login_from_provider = true;
                provider_login_succeeded = false;
                if let Some(root) = config_root.clone() {
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
                login_from_provider = true;
                provider_login_succeeded = false;
                if let Some(root) = config_root.clone() {
                    match LoginFlow::start(root, console, method) {
                        Ok(flow) => login = Some(flow),
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
                selected_chat = Some(chat);
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
                if draft_question == Some(id) {
                    draft_question = None;
                }
            }
            if let Some(id) = outcome.draft_question {
                draft_question = Some(id);
            }
            if let Some(snapshot) = outcome.restored {
                displayed_sequences.insert(snapshot.id, snapshot.last_sequence);
                tools.restore(&snapshot.conversation_events());
                sync_snapshot(&snapshot, &mut pending, &mut permissions, &mut questions);
            }
            if let Some((chat, selected_goal)) = outcome.selection {
                if selected_chat != Some(chat) {
                    message_groups = MessageGroups::default();
                    task_subscription = None;
                    transcript_dirty = true;
                }
                selected_chat = Some(chat);
                goal = selected_goal;
                draft_question = None;
            }
            if let Some(receipt) = outcome.receipt {
                selected_chat = Some(receipt.chat_id);
                goal = Some(receipt.loop_id);
                pending.insert(receipt.run_id, receipt);
            }
            if let Some(status) = outcome.status {
                console.set_status(&status)?;
                status_dirty = false;
            } else {
                status_dirty = true;
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
                if observation.render_in_chat {
                    let rendered_terminal =
                        render_user_shell_update(agent, console, &record, &mut user_shell_offsets)
                            .await?;
                    transcript_dirty |= rendered_terminal;
                }
                if user_shell_is_terminal(&record) {
                    user_shell_chats.remove(&record.id);
                    user_shell_offsets.remove(&record.id);
                }
            }
        }
        if interactive && status_dirty {
            running_activity = None;
            cached_activity_status = None;
            console.set_activity_interruptible(false)?;
            if overview_active {
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
                let snapshot = agent
                    .snapshot(chat)
                    .await
                    .map_err(|error| error.to_string())?;
                console.set_input_context(
                    Some(chat),
                    &snapshot.workdir,
                    &snapshot.additional_directories,
                )?;
                console.set_visible_delegation(snapshot.delegation.is_some())?;
                console.set_foreground_shell(foreground_shells.contains_key(&chat))?;
                console.set_queued_inputs(snapshot.queued_inputs())?;
                console.set_chat_images(
                    chat,
                    snapshot.accepts_images(),
                    snapshot.highest_pasted_image_id(),
                )?;
                reconcile_consumed_input_echoes(console, &snapshot)?;
                let task_snapshot = task_center
                    .snapshot(chat)
                    .await
                    .map_err(|error| error.to_string())?;
                console.update_status_area(
                    snapshot.policy.mode.to_string(),
                    safe_text(&snapshot.model.model),
                    context_usage(&snapshot),
                    task_status_items(&task_snapshot),
                    todo_status_items(&snapshot, goal),
                )?;
                let running_receipt = pending
                    .values()
                    .find(|receipt| {
                        receipt.chat_id == chat
                            && snapshot.loops.iter().any(|goal| {
                                goal.run_id == Some(receipt.run_id)
                                    && goal.state == tickloop_agent::runtime::LoopState::Running
                            })
                    })
                    .or_else(|| pending.values().find(|receipt| receipt.chat_id == chat));
                let running_loop = running_receipt.map(|receipt| receipt.loop_id);
                let run_is_working = running_loop.is_some();
                let mut activity: Option<Vec<(String, Option<Role>)>> = None;
                let connection = execution_connection_notice(
                    &global.projection().rows,
                    chat,
                    agent.disconnected().is_some(),
                    !snapshot.loops.is_empty()
                        && snapshot.loops.iter().all(|goal| {
                            goal.state == tickloop_agent::runtime::LoopState::Completed
                        })
                        && !pending.values().any(|receipt| receipt.chat_id == chat)
                        && agent.unknown_input(chat).is_none()
                        && snapshot.queued_inputs().is_empty()
                        && snapshot.user_shells.iter().all(|record| {
                            matches!(
                                record.state,
                                UserShellState::Completed | UserShellState::Failed
                            )
                        }),
                    owner_unreachable.is_some(),
                );
                if connection.is_some() {
                    console.set_execution_connection_notice(
                        connection,
                        agent.disconnected().or_else(|| owner_unreachable.clone()),
                    )?;
                } else if let Some(notice) = owner_unreachable.clone() {
                    console.set_connection_notice(Some(notice))?;
                } else {
                    console.set_execution_connection_notice(connection, agent.disconnected())?;
                }
                let state = if connection
                    .is_some_and(|notice| notice != console::ExecutionConnectionNotice::CachedView)
                {
                    String::new()
                } else if logout.is_some() {
                    "Removing Codex login · Esc cancels".to_owned()
                } else if !snapshot.permissions.is_empty() {
                    "Waiting for approval".to_owned()
                } else if !snapshot.questions.is_empty() {
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
                        label: working_activity(&snapshot, running_loop),
                        started_at: running_receipt
                            .and_then(|receipt| observed_run_starts.get(&receipt.run_id).copied()),
                        interruptible: owner_unreachable.is_none()
                            && agent.disconnected().is_none()
                            && global.projection().rows.iter().any(|row| {
                                row.chat_id == chat
                                    && row.availability
                                        == crate::agent_access::ExecutionAvailability::Live
                                    && row
                                        .capabilities
                                        .iter()
                                        .any(|capability| capability == "loop")
                            }),
                    };
                    let preview = pending.iter().find_map(|(run_id, receipt)| {
                        (receipt.chat_id == chat && thinking_runs.contains(run_id))
                            .then(|| thinking_previews.get(run_id))
                            .flatten()
                    });
                    activity = Some(working.spans(console, preview, &working_shimmer)?);
                    console.set_activity_interruptible(working.interruptible)?;
                    running_activity = Some(working);
                    "Working".to_owned()
                } else if foreground_shells.contains_key(&chat) {
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
                    if snapshot.pending_model.is_some() {
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
                let displayed_background =
                    background_wait(selected_chat, &pending, &permissions, &questions)
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
                            background_notice.clone().map(|(chat, notice)| {
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
                                agent.cached_title(chat),
                                notice.message,
                                notice.compact_message,
                                notice.necessary,
                                role,
                            )
                        })
                        .transpose()?,
                )?;
                cached_activity_status = Some((chat, safe_text(status.trim_start_matches(" · "))));
                console.set_activity_and_status(
                    activity.as_deref(),
                    &safe_text(status.trim_start_matches(" · ")),
                )?;
            } else {
                console.set_activity(None)?;
                console.set_background_status_notice(None)?;
                console.set_footer_items(Vec::new())?;
                if let Ok(draft) = agent.configuration(ConfigTarget::NewDraft).await {
                    console.update_status_area(
                        draft.execution_mode.to_string(),
                        safe_text(&draft.model.model),
                        None,
                        Vec::new(),
                        Vec::new(),
                    )?;
                }
            }
            status_dirty = false;
            activity_dirty = false;
        }
        if interactive && activity_dirty {
            if !overview_active
                && let Some(chat) = selected_chat
                && let Some((cached_chat, status)) = &cached_activity_status
                && *cached_chat == chat
            {
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
            }
            activity_dirty = false;
        }
        if login.is_none() && !overview_active {
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
                input_closed_while_waiting(selected_chat, &pending, &permissions, &questions)
        {
            return Err(message);
        }
        if pending.is_empty()
            && compactions.is_empty()
            && directed_summaries.is_empty()
            && deferred_events.is_empty()
            && !reply_lines.values().any(ReplyLines::has_complete_line)
            && login.is_none()
            && logout.is_none()
            && paused_inputs.is_empty()
            && menu_commands.is_empty()
            && eof
        {
            return if failed {
                Err("one or more tasks failed; review the errors above".into())
            } else {
                Ok(())
            };
        }
        let summary_input_locked =
            !directed_summaries.is_empty() || !summary_recovery_required.is_empty();
        let visual_pacing_pending = interactive
            && !overview_active
            && reply_lines
                .values()
                .any(|reply| Some(reply.chat_id) == selected_chat && reply.has_complete_line());
        let has_visible_running_task = interactive && console.has_visible_running_task()?;
        let prefers_reduced_motion =
            interactive && console.display_preferences()?.prefers_reduced_motion;
        let discovery_active = menus.discovery_active();
        let login_active = login.is_some();
        let logout_active = logout.is_some();
        #[cfg(test)]
        let global_refresh_enabled = agent.recovery_refresh_enabled();
        #[cfg(not(test))]
        let global_refresh_enabled = true;
        tokio::select! {
            result = menus.next_discovery(), if discovery_active => {
                menus.finish_discovery(agent, console, result).await?;
                status_dirty = true;
            },
            update = async { login.as_mut().unwrap().next().await }, if login_active => {
                match update {
                    Some(LoginUpdate::Device { address, code }) => login.as_ref().unwrap().show_device(console, &address, &code)?,
                    Some(LoginUpdate::Browser { address, callback }) => login.as_mut().unwrap().show_browser(console, address, callback)?,
                    Some(LoginUpdate::Finished(result)) => {
                        let flow = login.take().unwrap();
                        let provider_label = flow.label();
                        flow.close(console)?;
                        match result {
                            Ok(account) => {
                                provider_login_succeeded = true;
                                outln!(console, "{} login saved · {}", provider_label, safe_text(&account));
                            }
                            Err(error) => outln!(console, "Cannot log in: {}", safe_text(&error)),
                        }
                    }
                    None => {
                        login.take().unwrap().close(console)?;
                        outln!(console, "Sign-in ended before completion; use /provider to retry.");
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
                                Ok(flow) => login = Some(flow),
                                Err(error) => outln!(console, "Cannot log in: {}", safe_text(&error)),
                            }
                        }
                    }
                    Err(crate::auth::AuthError::Cancelled) => outln!(console, "Logout cancelled; saved login remains."),
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
                if !matches!(&line,Some(InputEvent::Line(input)|InputEvent::Prompt {text:input,..}) if !input.trim_start().starts_with('/')) {
                    merged_echoes.next_submission_block();
                }
                match line {
                None | Some(InputEvent::Eof) => {
                    if interactive { return Ok(()); }
                    eof = true;
                },
                Some(InputEvent::Error(error)) => return Err(safe_text(&error)),
                Some(InputEvent::OpenOverview) => {
                    menus.dismiss(console)?;
                    // Only a registered conversation is something to return to; a new draft has none.
                    let source=match selected_chat { Some(chat)=>agent.reference(chat).await.ok(), None=>None };
                    global.visible_ready(source);
                    // An unreachable owner leaves the cached catalog on screen with a stale label;
                    // the View, its drafts and its reading positions stay alive.
                    if let Err(error)=global.refresh().await {
                        owner_unreachable=Some(format!("{DISCONNECTED_CACHED_VIEW_NOTICE} · {}",safe_text(&error.to_string())));
                        console.set_connection_notice(owner_unreachable.clone())?;
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
                    if let Err(error)=agent.display_chat(selected_chat).await {
                        console.set_connection_notice(Some(format!("{VIEW_REGISTRATION_UNAVAILABLE_NOTICE} · {}",safe_line(&error.to_string()))))?;
                    }
                    if let Some(chat)=selected_chat {
                        match restore_view(console,agent,&mut tools,chat,displayed_sequences.get(&chat).copied().unwrap_or(0),&mut reply_lines).await {
                            Ok(observation)=>{
                                displayed_sequences.insert(chat,observation.snapshot.last_sequence);
                                sync_snapshot(&observation.snapshot,&mut pending,&mut permissions,&mut questions);
                            },
                            Err(error)=>console.set_status(&format!("Showing cached conversation · {}",safe_line(&error)))?,
                        }
                    }
                    status_dirty = true;
                },
                Some(InputEvent::OpenAgent(agent_id)) => {
                    if let Err(error)=global.refresh().await {
                        owner_unreachable=Some(format!("{DISCONNECTED_CACHED_VIEW_NOTICE} · {}",safe_text(&error.to_string())));
                        console.set_connection_notice(owner_unreachable.clone())?;
                        console.set_status("Cannot open the Agent while its owner is unreachable; the overview shows cached facts")?;
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
                                    selected_chat,
                                    visible: global.projection().visible,
                                });
                                console.set_status(&format!("Cannot open the Agent · {}",safe_line(&error)))?;
                                status_dirty = true;
                                continue;
                            },
                        };
                        if let Some(snapshot)=opened {
                            overview_active=false;selected_chat=Some(snapshot.id);
                            goal=selected_loops.get(&snapshot.id).copied().unwrap_or_else(||selected_loop(&snapshot));draft_question=None;task_subscription=None;
                            displayed_sequences.insert(snapshot.id,snapshot.last_sequence);
                            tools.restore(&snapshot.conversation_events());
                            sync_snapshot(&snapshot,&mut pending,&mut permissions,&mut questions);
                            message_groups=MessageGroups::default();
                            #[cfg(test)]
                            agent.record_recovery_fact(recovery_tests::RecoveryFact::AgentOpened {chat:snapshot.id});
                        }
                    } else {console.set_status("This Agent is no longer live; reopen its saved history to recover")?;}
                    status_dirty = true;
                },
                Some(InputEvent::Suspend | InputEvent::OpenFooter(_) | InputEvent::OpenStatusTask(_)
                    | InputEvent::StopStatusTask(_) | InputEvent::OpenRewind | InputEvent::TakeBackQueued { .. }
                    | InputEvent::OpenTranscript { .. } | InputEvent::ExpandChoice { .. }
                    | InputEvent::BackgroundForegroundShell | InputEvent::TaskSurface(_)
                    | InputEvent::Subscription(_) | InputEvent::Permissions(_)
                    | InputEvent::UserShellSubmitted(_) | InputEvent::CycleExecutionMode) if login.is_some() => {},
                Some(InputEvent::Escape) => {
                    if let Some(flow) = login.take() {
                        flow.close(console)?;
                        outln!(console, "Login cancelled.");
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
                Some(event) if logout_active => {
                    paused_inputs.push_back(event);
                    console.set_status("Logging out · input will run afterward")?;
                    status_dirty = true;
                },
                Some(InputEvent::Suspend) => {
                    console.suspend()?;
                    status_dirty=true;
                },
                Some(InputEvent::OpenFooter(item)) => {
                    let result = if let Some(chat) = selected_chat {
                        match item {
                            FooterItemId::Tasks => open_tasks(&task_center,console,chat,&mut task_subscription,None,true).await,
                            FooterItemId::Todo => Ok(()),
                            FooterItemId::Agents => Ok(()),
                        }
                    } else { Ok(()) };
                    console.finish_footer_open()?;
                    result?;
                },
                Some(InputEvent::OpenStatusTask(task_id)) => {
                    if let Some(chat) = selected_chat {
                        let result = open_tasks(&task_center,console,chat,&mut task_subscription,Some(task_id),true).await;
                        console.finish_footer_open()?;
                        result?;
                    }
                },
                Some(InputEvent::StopStatusTask(task_id)) => {
                    if let Some(chat) = selected_chat {
                        match task_center.dispatch(chat,task_id,tickloop_agent::runtime::ShellTaskAction::Stop).await {
                            Ok(_) => status_dirty=true,
                            Err(error) => console.set_status(&format!("Cannot stop Shell task: {}",safe_text(&error.to_string())))?,
                        }
                    }
                },
                Some(InputEvent::OpenRewind) => {
                    let is_busy=selected_chat.is_some_and(|chat| pending.values().any(|receipt|receipt.chat_id==chat)
                        || compaction_chats.values().any(|(candidate,_)|*candidate==chat)
                        || directed_summaries.contains(chat)
                        || summary_recovery_required.contains(&chat)
                        || foreground_shells.contains_key(&chat));
                    open_rewind(agent,console,&mut menus,selected_chat,is_busy).await?;
                },
                Some(InputEvent::Interrupt) => {
                    if let Some(flow) = login.take() {
                        flow.close(console)?;
                        outln!(console, "Login cancelled.");
                        status_dirty = true;
                        continue;
                    }
                    if console.close_transcript()? { continue; }
                    if overview_active {
                        if console.confirm_control_c_exit()? {return Ok(());}
                        continue;
                    }
                    if let Some(chat)=selected_chat
                        && let Some(command_id)=foreground_shells.get(&chat).copied()
                    {
                        match agent.stop_user_shell(chat,command_id).await {
                            Ok(_) => outln!(console,"\nStopped user shell {command_id}."),
                            Err(error) => outln!(console,"\nCannot stop user shell: {}",safe_text(&error.to_string())),
                        }
                        foreground_shells.remove(&chat);
                        console.set_foreground_shell(false)?;
                        status_dirty=true;
                        continue;
                    }
                    if let Some(chat)=selected_chat
                        && directed_summaries.contains(chat)
                    {
                        agent.interrupt(chat).await.map_err(|error|error.to_string())?;
                        outln!(console,"\nCancelling conversation summary…");
                        status_dirty=true;
                        continue;
                    }
                    if console.confirm_control_c_exit()? {return Ok(());}
                    if let Some(chat) = selected_chat
                        && pending.values().any(|receipt| receipt.chat_id == chat)
                    {
                        agent.interrupt(chat).await.map_err(|error|error.to_string())?;
                        stop_visual_pacing(console,chat,&pending,&mut thinking_previews,&mut reply_lines,&mut unpaced_runs)?;
                        status_dirty=true;
                    }
                },
                Some(InputEvent::InterruptActivity) => {
                    let Some(chat) = selected_chat.filter(|chat| running_activity.as_ref().is_some_and(|activity| activity.chat_id == *chat && activity.interruptible)) else { continue; };
                    if pending.values().any(|receipt| receipt.chat_id == chat) || directed_summaries.contains(chat) {
                        match agent.interrupt(chat).await {
                            Ok(_) => {
                                stop_visual_pacing(console,chat,&pending,&mut thinking_previews,&mut reply_lines,&mut unpaced_runs)?;
                                if directed_summaries.contains(chat) { outln!(console,"\nCancelling conversation summary…"); }
                            },
                            Err(error) => console.set_status(&format!("Cannot interrupt: {}",safe_text(&error.to_string())))?,
                        }
                        status_dirty=true;
                    }
                },
                Some(InputEvent::TakeBackQueued { draft_empty }) => {
                    let Some(chat) = selected_chat else {
                        console.set_status("No Chat is selected")?;
                        continue;
                    };
                    match agent.take_queued_inputs(chat, draft_empty).await {
                        Ok(entries) if entries.is_empty() => {
                            let snapshot=agent.snapshot(chat).await.map_err(|error|error.to_string())?;
                            console.set_queued_inputs(snapshot.queued_inputs())?;
                        },
                        Ok(entries) => {
                            for entry in &entries {
                                pending.remove(&entry.id);
                                user_shell_offsets.remove(&entry.id);
                                user_shell_chats.remove(&entry.id);
                                foreground_shells.remove(&entry.chat_id);
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
                        let snapshot=agent.snapshot(chat).await.map_err(|error|error.to_string())?;
                        console.update_transcript(id,presentation::transcript_document(&snapshot, console.display_profile()?, &console.image_links()?))?;
                    } else {console.update_transcript(id,vec!["No conversation yet. Return to enter a task.".into()].into())?;}
                    transcript_dirty=false;
                },
                Some(InputEvent::PickerAction { menu_id, index, action }) => {
                    if login.is_none() && logout.is_none() {
                        match menus.picker_action(agent, console, menu_id, index, action).await {
                            Ok(outcome) => menu_outcome = Some(outcome),
                            Err(error) => outln!(console, "Cannot apply selection: {}", safe_text(&error)),
                        }
                        status_dirty = true;
                    }
                },
                Some(InputEvent::Choice {menu_id,index,comment}) => {
                    if login.as_ref().is_some_and(|flow| flow.id() == menu_id) {
                        if index == 0 && login.as_ref().unwrap().is_browser() {
                            login.as_ref().unwrap().submit_callback(console, comment.as_deref())?;
                            console.finish_menu_submission(menu_id)?;
                            continue;
                        }
                        login.take().unwrap().close(console)?;
                        outln!(console, "Login cancelled.");
                        status_dirty = true;
                        continue;
                    }
                    status_dirty=true;
                    match menus.choose(agent,console,menu_id,Some(index),comment).await {
                        Ok(outcome)=>menu_outcome=Some(outcome),
                        Err(error)=>outln!(console,"Cannot apply selection: {}",safe_text(&error)),
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
                    if login.as_ref().is_some_and(|flow| flow.id() == menu_id) {
                        login.take().unwrap().close(console)?;
                        outln!(console, "Login cancelled.");
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
                    if let Some(chat)=selected_chat
                        && let Some(command_id)=foreground_shells.get(&chat).copied()
                    {
                        match agent.background_user_shell(chat,command_id).await {
                            Ok(_) => {
                                foreground_shells.remove(&chat);
                                backgrounded_shells.insert(command_id);
                                console.set_foreground_shell(false)?;
                                outln!(console,"\nUser shell {command_id} continues in the background.");
                            },
                            Err(error)=>outln!(console,"Cannot background user shell: {}",safe_text(&error.to_string())),
                        }
                        status_dirty=true;
                    }
                },
                Some(InputEvent::TaskSurface(effect)) => {
                    let Some(chat)=console.task_surface_chat()? else {continue;};
                    apply_task_surface_effect(&task_center,console,chat,effect).await?;
                },
                Some(InputEvent::Subscription(effect)) => {
                    let Some(chat)=console.subscription_view_chat()? else {continue;};
                    apply_subscription_effect(agent,console,chat,effect).await?;
                },
                Some(InputEvent::Permissions(effect)) => {
                    match effect {
                        PermissionsEffect::Close { destination: PermissionsReturn::ModePicker { settings } } => {
                            menus.permission_modes(agent, console, ConfigTarget::visible(selected_chat), settings).await?;
                        },
                        PermissionsEffect::None => {},
                        PermissionsEffect::Apply(change) => match console.permissions_subject()? {
                            Some(PermissionsSubject::Agent { chat, revision }) => {
                                match agent.apply_permission_change(chat,change,revision).await {
                                    Ok(view)=>console.permissions_saved(view)?,
                                    Err(tickloop_agent::runtime::PermissionChangeError::Conflict { latest }) => console.permissions_conflict(*latest)?,
                                    Err(error)=>console.permissions_error(safe_text(&error.to_string()))?,
                                }
                            },
                            // Saved rules and Auto policy are written now; the mode, session rules
                            // and directories wait for the first input. No Agent is created.
                            Some(PermissionsSubject::NewDraft { project, revision }) => {
                                match agent.change_draft_permissions(&project,&revision,change).await {
                                    Ok(change)=>{
                                        let kept=change.kept;
                                        console.permissions_saved(change.page)?;
                                        if kept {console.permissions_notice("Kept for the New Agent; applies when you send the first input".into())?;}
                                    },
                                    Err(DraftPermissionsError::Conflict(latest))=>console.permissions_conflict(*latest)?,
                                    Err(DraftPermissionsError::Rejected(error))=>console.permissions_error(safe_text(&error.to_string()))?,
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
                                    goal=Some(receipt.loop_id);
                                    pending.insert(receipt.run_id,receipt);
                                    transcript_dirty=true;
                                    status_dirty=true;
                                    outln!(console,"New retry requested; the Agent will prepare the operation and check current permissions.");
                                },
                                Err(error)=>console.permissions_denials(chat,Err(safe_text(&error.to_string())))?,
                            }
                        },
                        PermissionsEffect::OpenJudgmentSettings { select_reviewer } => {
                            let Some(subject)=console.permissions_subject()? else {continue;};
                            if let Err(error) = menus.judgment_settings_from_permissions(agent,console,subject,select_reviewer).await {
                                console.permissions_error(format!("Cannot open Judgment settings: {}",safe_text(&error)))?;
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
                        let receipt=global.dispatch(agent.launch_spec(),crate::agent_access::UserInput::Shell { command:submission.command().into(),background:true },agent.origin()).await;
                        match receipt {
                            Ok(receipt)=>match agent.accept_created(receipt).await {
                                CreationAcceptance::Attached(_)=>{},
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
                    let chat = match ensure_chat(console,agent,&mut selected_chat,&directory).await {
                        Ok(chat)=>chat,
                        Err(error)=>{console.restore_rejected_shell(submission.command(),&error)?;continue;},
                    };
                    let history_line=format!("!{}",submission.command());
                    // Only this attempt's own echo may be withdrawn if it is held back unsent.
                    let echo=if retry_of.is_none() {console.record_input(&history_line)?} else {None};
                    console.record_prompt_history(&history_line)?;
                    let starts_in_background = submission.execution()==shell_input::ShellExecution::Background;
                    let sent=send_input(agent,chat,retry_of,agent.submit_user_shell_for_goal(chat,goal,submission.command().to_owned(),starts_in_background),|result|match result {
                        crate::agent_access::CommandResult::ShellSubmitted(receipt)=>Some(receipt),
                        _=>None,
                    }).await;
                    match sent {
                        InputSend::Accepted { value: receipt, adopted }=>{
                            if let Some(id)=adopted {console.set_status(&input_adopted(id))?;}
                            user_shell_offsets.insert(receipt.command_id,0);
                            user_shell_chats.insert(receipt.command_id,chat);
                            outln!(
                                console,
                                "\n{} Shell {}\n  $ {}",
                                console.display_profile()?.glyph(Glyph::ToolCall),
                                receipt.command_id,
                                safe_text(submission.command())
                            );
                            let is_running = agent
                                .snapshot(chat)
                                .await
                                .map_err(|error| error.to_string())?
                                .user_shells
                                .iter()
                                .any(|record| {
                                    record.id == receipt.command_id
                                        && record.state == UserShellState::Running
                                });
                            if !starts_in_background && is_running {
                                foreground_shells.insert(chat,receipt.command_id);
                                console.set_foreground_shell(true)?;
                            }
                        },
                        InputSend::Unknown(id, error)=>{
                            outln!(console,"Cannot run user shell: {}",safe_text(&error.to_string()));
                            console.restore_unknown_shell(submission.command(),&safe_text(&error.to_string()),Some(id))?;
                        },
                        InputSend::Held(error)=>{
                            if let Some(echo)=echo {console.withdraw_echo(Some(chat),echo)?;}
                            outln!(console,"Cannot run user shell: {}",safe_text(&error.to_string()));
                            console.restore_rejected_shell(submission.command(),&safe_text(&error.to_string()))?;
                        },
                        InputSend::Failed(error)=>{
                            outln!(console,"Cannot run user shell: {}",safe_text(&error.to_string()));
                            console.restore_rejected_shell(submission.command(),&safe_text(&error.to_string()))?;
                        },
                    }
                    status_dirty=true;
                },
                Some(input @ (InputEvent::Line(_) | InputEvent::Prompt { .. })) => {
                    let retry_of = console.take_submitted_retry()?;
                    let (line, file_references, pasted_images, restored_contexts, is_user_prompt) = match input {
                        InputEvent::Line(line) => (line, Vec::new(), Vec::new(), Vec::new(), false),
                        InputEvent::Prompt { text, file_references, images } => {
                            let restored_contexts = if is_branch_command(text.trim()) {
                                Vec::new()
                            } else {
                                console.take_restored_contexts(&text)?
                            };
                            (text, file_references, images, restored_contexts, true)
                        }
                        _ => unreachable!(),
                    };
                    if login.is_some() {
                        if matches!(line.trim(), "cancel" | "/cancel" | "/exit") {
                            login.take().unwrap().close(console)?;
                            outln!(console, "Login cancelled.");
                            if line.trim() == "/exit" { return Ok(()); }
                        } else if login.as_ref().unwrap().is_browser() {
                            login.as_ref().unwrap().submit_callback(console, Some(&line))?;
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
                            match global.interpret(crate::agent_console::Intent::ChooseLocation {generation,view:crate::agent_access::ViewRef{session_id,view_id}}) {
                                Ok(Some(effect))=>{if let Some(snapshot)=navigate_agent(&mut global,agent,console,&mut menus,effect,&displayed_sequences,&mut reply_lines).await? {
                                    selected_chat=Some(snapshot.id);goal=selected_loops.get(&snapshot.id).copied().unwrap_or_else(||selected_loop(&snapshot));overview_active=false;
                                    displayed_sequences.insert(snapshot.id,snapshot.last_sequence);
                                    tools.restore(&snapshot.conversation_events());
                                    sync_snapshot(&snapshot,&mut pending,&mut permissions,&mut questions);
                                    message_groups=MessageGroups::default();
                                }},
                                Ok(None)=>{},Err(error)=>console.set_status(&safe_text(&error.to_string()))?,
                            }
                        }
                        continue;
                    }
                    if overview_active {
                        if line.trim()=="/exit" {return Ok(());}
                        // The reference is shown in place; only a command the overview cannot name becomes a Skill dispatch.
                        if line.trim()=="/help advanced" {menus.help_advanced(console)?;continue;}
                        if let Some(command_id)=line.trim().strip_prefix("/acknowledge-create ") {
                            match command_id.parse::<uuid::Uuid>().map_err(|error|error.to_string())
                                .and_then(|id|global.interpret(crate::agent_console::Intent::AcknowledgeUnconfirmedDispatch(id)).map_err(|error|error.to_string())) {
                                Ok(_)=>{console.set_creation_notice(None)?;console.set_status("Unknown creation acknowledged; no command was replayed")?;},
                                Err(error)=>console.restore_rejected_prompt(restored_contexts,&error)?,
                            }
                            continue;
                        }
                        if line.trim()=="/clear" {
                            global.interpret(crate::agent_console::Intent::NewDraft).map_err(|error|error.to_string())?;
                            console.set_global_overview(None)?;console.clear_visible_agent()?;
                            overview_active=false;selected_chat=None;goal=None;
                            agent.display_chat(None).await.map_err(|error|error.to_string())?;
                            status_dirty=true;continue;
                        }
                        if line.trim()=="/resume" {
                            let directory=global.projection().dispatch_directory;
                            match Workflow::chats(agent,&directory,None,false).await {
                                Ok(workflow)=>{
                                    console.set_global_overview(None)?;global.interpret(crate::agent_console::Intent::Back).map_err(|error|error.to_string())?;
                                    overview_active=false;history_return_overview=true;menus.conversations(console,workflow)?;
                                },
                                Err(error)=>console.restore_rejected_prompt(restored_contexts,&error)?,
                            }
                            continue;
                        }
                        if line.trim().starts_with("/resume ") {
                            match open_chat(agent,line.trim(),None).await {
                                Ok(snapshot)=>{
                                    console.set_global_overview(None)?;global.interpret(crate::agent_console::Intent::Back).map_err(|error|error.to_string())?;
                                    overview_active=false;goal=show_opened_chat(console,agent,&snapshot,true)?;selected_chat=Some(snapshot.id);
                                    sync_snapshot(&snapshot,&mut pending,&mut permissions,&mut questions);
                                },
                                Err(error)=>console.restore_rejected_prompt(restored_contexts,&error)?,
                            }
                            continue;
                        }
                        if let Some(path)=line.trim().strip_prefix("/cwd ") {
                            let directory=PathBuf::from(path);
                            let directory=if directory.is_absolute(){directory}else{global.projection().dispatch_directory.join(directory)};
                            match directory.canonicalize() {
                                Ok(directory) if directory.is_dir()=>{global.interpret(crate::agent_console::Intent::DispatchDirectory(directory)).map_err(|error|error.to_string())?;},
                                _=>{console.restore_rejected_prompt(restored_contexts,"Dispatch directory must exist")?;continue;}
                            }
                            global_refresh.reset_immediately();status_dirty=true;continue;
                        }
                        if line.trim()=="/cwd" {
                            console.set_status(&format!("New Agents start in {}",safe_line(&global.projection().dispatch_directory.display().to_string())))?;
                            continue;
                        }
                        if let Some((command,reason))=completion::unavailable_command(&line,completion::CommandScope::Overview) {
                            console.restore_rejected_prompt(restored_contexts,&completion::command_refusal(console.command_scope(),command,reason))?;
                            continue;
                        }
                        let input=if let Some(skill)=line.trim().strip_prefix('/') {
                            let (name,arguments)=skill.split_once(' ').unwrap_or((skill,""));
                            crate::agent_access::UserInput::Skill {name:name.into(),arguments:arguments.into()}
                        } else {
                            crate::agent_access::UserInput::Text {text:line.clone(),contexts:pasted_images.into_iter().map(InputContext::Image).chain(restored_contexts.clone()).collect(),paths:file_references}
                        };
                        console.record_input(&line)?;console.record_prompt_history(&line)?;
                        if let Err(error)=agent.refresh_interaction_source().await {
                            console.restore_rejected_prompt(restored_contexts,&safe_line(&error.to_string()))?;
                            continue;
                        }
                        match global.dispatch(agent.launch_spec(),input,agent.origin()).await {
                            Ok(receipt)=>match agent.accept_created(receipt).await {
                                CreationAcceptance::Attached(submitted)=>{if let Some(receipt)=submitted {pending.insert(receipt.run_id,receipt);}},
                                CreationAcceptance::AttachUnavailable(error)=>console.set_status(&creation_attach_unavailable(&error))?,
                                CreationAcceptance::Rejected(error)=>console.restore_rejected_prompt(restored_contexts,&format!("New Agent was not accepted: {error}"))?,
                            },
                            Err(error)=>console.restore_rejected_prompt(restored_contexts,&format!("New Agent acceptance is unconfirmed: {error}; check the receipt before another submission"))?,
                        }
                        show_creation_feedback(console,&global.projection())?;
                        global_refresh.reset_immediately();
                        status_dirty=true;continue;
                    }
                    if selected_chat.is_some() && agent.disconnected().is_some()
                        && !matches!(line.trim(),"/exit"|"/clear"|"/status"|"/help"|"/help advanced"|"/resume"|"/tui"|"/details")
                        && !line.trim().starts_with("/resume ") {
                        console.restore_rejected_prompt(restored_contexts,"This execution is unavailable; inspect cached history or use /resume explicitly")?;
                        continue;
                    }
                    // Retrying an unchanged draft checks its receipt without another optimistic echo.
                    let mut echo=if retry_of.is_none() {console.record_input(&line)?} else {None};
                    let record_after_chat = is_user_prompt
                        && selected_chat.is_none()
                        && !line.trim_start().starts_with('/');
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
                        && line.starts_with('/')
                        && command_timing(&line) == CommandTiming::AfterTurn
                        && let Some(chat) = selected_chat
                    {
                        let snapshot=agent.snapshot(chat).await.map_err(|error|error.to_string())?;
                        let is_busy=snapshot.loops.iter().any(|goal|matches!(goal.state,tickloop_agent::runtime::LoopState::Running|tickloop_agent::runtime::LoopState::WaitingPermission|tickloop_agent::runtime::LoopState::WaitingQuestion));
                        if is_busy {
                            let queued=agent.queue_command(chat,goal,original_input.clone());
                            match send_input(agent,chat,retry_of,queued,|result|match result {
                                crate::agent_access::CommandResult::Queued { command_id }=>Some(command_id),
                                _=>None,
                            }).await {
                                InputSend::Accepted { adopted, .. }=>{
                                    if let Some(echo)=echo {console.withdraw_echo(Some(chat),echo)?;}
                                    console.set_status(&adopted.map(input_adopted).unwrap_or_else(||"Command queued until the current turn ends".into()))?;
                                },
                                InputSend::Unknown(id, error)=>{
                                    if let Some(echo)=echo {console.withdraw_echo(Some(chat),echo)?;}
                                    console.restore_unknown_prompt(restored_contexts,&safe_line(&error.to_string()),Some(id))?;
                                },
                                InputSend::Held(error)=>{
                                    if retry_of.is_none() && let Some(echo)=echo {console.withdraw_echo(Some(chat),echo)?;}
                                    console.restore_unknown_prompt(restored_contexts,&safe_line(&error.to_string()),None)?;
                                },
                                InputSend::Failed(error)=>console.restore_unknown_prompt(restored_contexts,&safe_line(&error.to_string()),None)?,
                            }
                            status_dirty=true;
                            continue;
                        }
                    }
                    if !interactive && let Some(menu_id)=menus.id() && !line.starts_with('/') {
                        let selection=if line.eq_ignore_ascii_case("cancel") {Some(None)} else {line.parse::<usize>().ok().and_then(|number|number.checked_sub(1)).map(Some)};
                        if let Some(index)=selection {
                            match menus.choose(agent,console,menu_id,index,None).await {
                                Ok(outcome)=>menu_outcome=Some(outcome),
                                Err(error)=>outln!(console,"Cannot apply selection: {}",safe_text(&error)),
                            }
                        } else {outln!(console,"Enter an option number or cancel; no selection was applied.");}
                        continue;
                    }
                    if should_dismiss_menu_before_dispatch(&line) {menus.dismiss(console)?;}
                    match line.as_str() {
                        "/exit" => return Ok(()),
                        "/help" | "/" if interactive => menus.help(console,None)?,
                        "/help" => outln!(console, "{}",help_text()),
                        "/help advanced" if interactive => menus.help_advanced(console)?,
                        "/help advanced" => outln!(console, "{}",completion::command_help(true)),
                        command if command == "/artifacts" || command.starts_with("/artifacts ") => {
                            let Some(root) = config_root.as_ref() else {
                                outln!(console, "Artifact Service needs a state directory; set TICKLOOP_CONFIG_DIR.");
                                continue;
                            };
                            match command.strip_prefix("/artifacts").unwrap_or_default().trim() {
                                "login" => {
                                    match LoginFlow::start(root.clone(), console, login::LoginMethod::Artifact) {
                                        Ok(flow) => login = Some(flow),
                                        Err(error) => outln!(console, "Cannot start Artifact Service login: {}", safe_text(&error)),
                                    }
                                }
                                "status" => {
                                    match tickloop_agent::artifacts::ArtifactClient::new(root) {
                                        Ok(client) => match client.status().await {
                                            Ok(Some(user)) => outln!(console, "Artifact Service account: {} · {}", safe_text(user.display_name()), safe_text(client.origin())),
                                            Ok(None) => outln!(console, "Artifact Service: signed out · {}. Use /artifacts login.", safe_text(client.origin())),
                                            Err(error) => outln!(console, "Cannot check Artifact Service account: {}", safe_text(&error.to_string())),
                                        },
                                        Err(error) => outln!(console, "Cannot configure Artifact Service: {}", safe_text(&error.to_string())),
                                    }
                                }
                                "logout" => {
                                    if let Err(error) = agent.disable_event_sources().await {
                                        outln!(console, "Cannot stop Artifact feedback listening before logout: {}", safe_text(&error.to_string()));
                                        continue;
                                    }
                                    match tickloop_agent::artifacts::ArtifactClient::new(root) {
                                        Ok(client) => match client.logout().await {
                                            Ok(tickloop_agent::artifacts::LogoutOutcome::Removed) => outln!(console, "Artifact Service login revoked or invalidated and removed locally."),
                                            Ok(tickloop_agent::artifacts::LogoutOutcome::NewerLoginPresent) => outln!(console, "Previous Artifact Service login revoked; a newer local login remains active."),
                                            Ok(tickloop_agent::artifacts::LogoutOutcome::AlreadySignedOut) => outln!(console, "Artifact Service is already signed out."),
                                            Err(error) => outln!(console, "Cannot log out of Artifact Service: {}", safe_text(&error.to_string())),
                                        },
                                        Err(error) => outln!(console, "Cannot configure Artifact Service: {}", safe_text(&error.to_string())),
                                    }
                                }
                                subcommand if subcommand.starts_with("resume ") => {
                                    let Some(chat_id) = selected_chat else {
                                        outln!(console, "Select a Chat before resuming feedback listening.");
                                        continue;
                                    };
                                    let loop_id = subcommand.strip_prefix("resume ").unwrap_or_default().trim().parse::<uuid::Uuid>();
                                    match loop_id {
                                        Ok(loop_id) => match agent.resume_event_sources(chat_id, loop_id).await {
                                            Ok(count) => outln!(console, "Resumed feedback listening for Loop {} ({} source{}).", loop_id, count, if count == 1 { "" } else { "s" }),
                                            Err(error) => outln!(console, "Cannot resume feedback listening: {}", safe_text(&error.to_string())),
                                        },
                                        Err(_) => outln!(console, "Use /artifacts resume LOOP_ID with the exact Loop ID shown in /loops details."),
                                    }
                                }
                                _ => outln!(console, "Use /artifacts login, /artifacts status, /artifacts logout or /artifacts resume LOOP_ID."),
                            }
                        },
                        command if is_rewind_command(command) => {
                            let is_busy=selected_chat.is_some_and(|chat| pending.values().any(|receipt|receipt.chat_id==chat)
                                || compaction_chats.values().any(|(candidate,_)|*candidate==chat)
                                || directed_summaries.contains(chat)
                                || summary_recovery_required.contains(&chat)
                                || foreground_shells.contains_key(&chat));
                            if open_rewind(agent,console,&mut menus,selected_chat,is_busy).await?.is_some() {
                                status_dirty=false;
                            }
                        },
                        "/clear" => {
                            if let Some(chat)=selected_chat {
                                let snapshot=agent.snapshot(chat).await.map_err(|error|error.to_string())?;
                                if snapshot.delegation.is_some() {
                                    console.set_status("Unsupported: child history and derivation commands are not available")?;
                                    continue;
                                }
                                let mut shell_stop_failed=false;
                                let tasks=task_center.snapshot(chat).await.map_err(|error|error.to_string())?;
                                for shell in tasks.tasks.iter().filter(|task|task.chat_id==chat && agent.disconnected().is_none() && !task.was_background && task.capabilities.can_stop && matches!(task.state,ShellTaskState::Running|ShellTaskState::Queued)) {
                                    if let Err(error)=task_center.dispatch(chat,shell.id,tickloop_agent::runtime::ShellTaskAction::Stop).await {
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
                            goal=None;
                            draft_question=None;
                            task_subscription=None;
                            message_groups=MessageGroups::default();
                            tools.restore(&[]);
                            transcript_dirty=true;
                        },
                        "/status" => {
                            if let Some(chat)=selected_chat {
                                let snapshot=agent.snapshot(chat).await.map_err(|error|error.to_string())?;
                                outln!(console,"Chat {} · {} · {:?} · {} · {} queued",chat,safe_text(&snapshot.model.model),snapshot.model.protocol,snapshot.policy.mode,snapshot.queued_inputs().len());
                            } else {outln!(console,"No Chat is selected.");}
                        },
                        "/resume" => match Workflow::chats(agent,&workdir,selected_chat,false).await {
                            Ok(picker) => menus.conversations(console,picker)?,
                            Err(error) => outln!(console,"{error}"),
                        },
                        "/loops" if interactive => {
                            if let Some(chat)=selected_chat {menus.workflow(console,Workflow::goals(agent,chat).await?)?;}
                            else {outln!(console,"No Chat is selected. Enter a task first.");}
                        },
                        command if interactive && is_task_surface_command(command) => {
                            if let Some(chat)=selected_chat {
                                open_tasks(&task_center,console,chat,&mut task_subscription,None,false).await?;
                            }
                            else {outln!(console,"No Chat is selected. Enter a task first.");}
                        },
                        command if interactive && is_subscription_view_command(command) => {
                            if let Some(chat)=selected_chat {
                                open_subscriptions(agent,console,chat).await?;
                            }
                            else {outln!(console,"No Chat is selected. Enter a task first.");}
                        },
                        "/plan" => {
                            let Some(chat)=selected_chat else {
                                agent.change_configuration(ConfigTarget::NewDraft,ConfigurationChange::ExecutionMode {mode:ExecutionMode::Plan}).await.map_err(|error|error.to_string())?;
                                outln!(console,"Plan mode is active. Enter a task to make a plan.");
                                status_dirty=true;
                                continue;
                            };
                            agent.set_execution_mode(chat,ExecutionMode::Plan).await.map_err(|error|error.to_string())?;
                            if let Some(selected_goal)=goal {
                                let snapshot=agent.snapshot(chat).await.map_err(|error|error.to_string())?;
                                if let Some(plan)=snapshot.plans.iter().find(|plan| plan.loop_id==selected_goal) {
                                    if interactive {
                                    match Workflow::plan(agent,chat,selected_goal).await {
                                        Ok(workflow)=>menus.workflow(console,workflow)?,
                                        Err(error)=>outln!(console,"Cannot open plan: {}",safe_text(&error)),
                                    }
                                    } else {
                                        outln!(console,"Plan: {} · version {} · approved mode {:?}\n{}",plan.loop_id,plan.version,plan.approved_execution_mode,safe_text(&plan.markdown));
                                    }
                                } else {outln!(console,"Plan mode is active. Enter a task to make a plan.");}
                            } else {outln!(console,"Plan mode is active. Enter a task to make a plan.");}
                        },
                        "/details" if interactive => {
                            let id = console.start_transcript()?;
                            if let Some(chat)=selected_chat {
                                let snapshot=agent.snapshot(chat).await.map_err(|error|error.to_string())?;
                                console.update_transcript(id,presentation::transcript_document(&snapshot, console.display_profile()?, &console.image_links()?))?;
                            } else {console.update_transcript(id,vec!["No conversation yet. Return to enter a task.".into()].into())?;}
                            transcript_dirty=false;
                        },
                        "/details" => out!(console, "{}",tools.toggle()),
                        "/diff" if interactive => {
                            if let Some(chat) = selected_chat {
                                open_diff(agent,console,chat).await?;
                            } else {
                                outln!(console, "No Chat is selected. Enter a task first.");
                            }
                        },
                        "/diff" => {
                            if let Some(chat) = selected_chat {
                                let snapshot = agent.snapshot(chat).await.map_err(|error|error.to_string())?;
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
                            menus.settings(agent,console,ConfigTarget::visible(selected_chat)).await?;
                        },
                        "/permissions" => {
                            if console.is_interactive() {
                                if let Err(error)=menus.permission_modes(agent,console,ConfigTarget::visible(selected_chat),false).await {
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
                            match agent.permissions_view(chat).await {
                                Ok(view) => {
                                    let rules=view.rules.into_iter().filter(|entry| matches!(entry.source,tickloop_agent::runtime::PermissionRuleSource::ChatSession | tickloop_agent::runtime::PermissionRuleSource::ProjectUser)).map(|entry|entry.rule).collect::<Vec<_>>();
                                    outln!(console,"Permission rules (deny takes precedence over ask and allow):\n{}",safe_text(&serde_json::to_string_pretty(&rules).map_err(|_|"cannot render permission rules")?));
                                },
                                Err(error) => outln!(console,"Cannot list permission rules: {}",safe_text(&error.to_string())),
                            }
                        },
                        "" => {},
                        command if is_branch_command(command) => {
                            let preserve_until_summary_finishes=selected_chat
                                .is_some_and(|chat| directed_summaries.contains(chat));
                            match branch_from_terminal(console,agent,selected_chat,goal,command,preserve_until_summary_finishes).await {
                                Ok(outcome) => {
                                    menus.dismiss(console)?;
                                    outln!(console,"Branched conversation \"{}\". You are now in the new Chat {}. Use /resume {} to return to the original.",safe_line(&outcome.name),outcome.child_chat_id,outcome.source_chat_id);
                                    workflows::write_public_history(console, &outcome.snapshot)?;
                                    selected_chat=Some(outcome.child_chat_id);
                                    goal=None;
                                    draft_question=None;
                                    task_subscription=None;
                                    message_groups=MessageGroups::default();
                                    displayed_sequences.insert(outcome.child_chat_id,outcome.snapshot.last_sequence);
                                    tools.restore(&outcome.snapshot.conversation_events());
                                    sync_snapshot(&outcome.snapshot,&mut pending,&mut permissions,&mut questions);
                                    console.scroll_conversation_to_end()?;
                                    transcript_dirty=true;
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
                            let target=ConfigTarget::visible(selected_chat);
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
                            menus.providers(agent, console, ConfigTarget::visible(selected_chat)).await?;
                        },
                        _ if line=="/model" || line.starts_with("/model ") => {
                            let target=ConfigTarget::visible(selected_chat);
                            if let Some(path) = line.strip_prefix("/model ") {
                                let result = async {
                                    let (config, provider) = read_config_with_provider(std::path::Path::new(path.trim()), user_settings.as_ref())?;
                                    model_client(config.clone(), config_root.as_deref()).map_err(|error|error.to_string())?;
                                    let previous = if let (Some(settings), Some(provider)) = (&user_settings, &provider) {
                                        Some(settings.set_transient_provider(provider.clone()))
                                    } else {
                                        None
                                    };
                                    let change = agent.change_configuration(target,ConfigurationChange::Model {config:config.clone(),thinking:None}).await;
                                    if change.is_ok() && target==ConfigTarget::NewDraft && let Some(provider)=&provider {
                                        agent.stage_transient_provider(provider.clone());
                                    }
                                    if change.is_err()
                                        && let (Some(settings), Some(provider), Some(previous)) = (&user_settings, &provider, previous)
                                    {
                                        settings.restore_transient_provider(&provider.id, previous);
                                    }
                                    Ok::<_,String>((change.map_err(|error|error.to_string())?,config,provider))
                                }.await;
                                match result {
                                    Ok((change,config,provider)) => { if user_settings.is_none() && let Some(provider) = provider { menus.add_transient_provider(provider); } outln!(console, "Model selection: {change} · {} · {} · {:?}",safe_text(&config.name),safe_text(&config.model),config.protocol); status_dirty=true; },
                                    Err(error) => outln!(console, "Cannot select model: {}",safe_text(&error)),
                                }
                            } else {
                                menus.models(agent,console,target).await?;
                            }
                        },
                        _ if line=="/thinking" || line.starts_with("/thinking ") => {
                            let target=ConfigTarget::visible(selected_chat);
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
                                        Ok(change) => outln!(console, "Thinking: {}{}", effort.map_or("auto", ReasoningEffort::label), change.staged_note()),
                                        Err(error) => outln!(console, "Cannot set Thinking: {}", safe_text(&error.to_string())),
                                    }
                                } else {
                                    let levels = supported.iter().map(|effort| effort.label()).collect::<Vec<_>>().join(", ");
                                    if levels.is_empty() {
                                        outln!(console, "{} does not support configurable Thinking.", safe_text(&configuration.model.model));
                                    } else {
                                        outln!(console, "Thinking levels for {}: {}. Use auto for the model default; use /settings to toggle Thinking.", safe_text(&configuration.model.model), levels);
                                    }
                                }
                            } else {
                                if let Err(error) = menus.efforts(agent, console, target).await {
                                    outln!(console, "Cannot open Thinking: {}", safe_text(&error));
                                }
                            }
                        },
                        _ if line.starts_with("/search-config ") => {
                            let target=ConfigTarget::visible(selected_chat);
                            let path=line.trim_start_matches("/search-config ").trim();
                            let result=async {
                                let config=if path=="off" {None} else {
                                    let bytes=std::fs::read(path).map_err(|_|"cannot read search configuration file")?;
                                    Some(serde_json::from_slice::<tickloop_agent::tools::SearchConfig>(&bytes).map_err(|_|"invalid search configuration JSON; provide endpoint and optional api_key_env")?)
                                };
                                agent.change_configuration(target,ConfigurationChange::Search {config:config.clone()}).await.map_err(|error|error.to_string())?;
                                Ok::<_,String>(config)
                            }.await;
                            match result {
                                Ok(Some(config))=>outln!(console, "Search service configured: {}",safe_text(&config.endpoint)),
                                Ok(None)=>outln!(console, "Search service disabled."),
                                Err(error)=>outln!(console, "Cannot configure search: {}",safe_text(&error)),
                            }
                        },
                        _ if line=="/compact" || line.starts_with("/compact ") => {
                            if let Some(chat)=selected_chat {
                                let agent=agent.clone();
                                let instructions=line.strip_prefix("/compact").map(str::trim).filter(|instructions|!instructions.is_empty()).map(str::to_owned);
                                let task=compactions.spawn(async move {(chat,agent.compact_with_instructions(chat,instructions).await)});
                                compaction_chats.insert(task.id(),(chat,None::<uuid::Uuid>));
                                outln!(console, "Compaction requested for Chat {chat}.");
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
                                        outln!(console, "Working directory: {}",safe_text(&directory.to_string_lossy()));
                                        for directory in additional {outln!(console, "Additional directory: {}",safe_text(&directory.to_string_lossy()));}
                                    },
                                    Err(error) => outln!(console, "Cannot update directories: {}",safe_text(&error)),
                                }
                                continue;
                            };
                            let mut snapshot = agent.snapshot(chat).await.map_err(|error|error.to_string())?;
                            if line=="/cwd" {
                                outln!(console, "Working directory: {}",safe_text(&snapshot.workdir.to_string_lossy()));
                                for directory in snapshot.additional_directories {outln!(console, "Additional directory: {}",safe_text(&directory.to_string_lossy()));}
                                continue;
                            }
                            if let Some(path) = line.strip_prefix("/cwd ") { snapshot.workdir = snapshot.workdir.join(path.trim()); }
                            if let Some(path) = line.strip_prefix("/add-dir ") { snapshot.additional_directories.push(snapshot.workdir.join(path.trim())); }
                            match agent.set_directories(chat,snapshot.workdir,snapshot.additional_directories).await {
                                Ok(()) => {
                                    let snapshot = agent.snapshot(chat).await.map_err(|error|error.to_string())?;
                                    outln!(console, "Working directory: {}",safe_text(&snapshot.workdir.to_string_lossy()));
                                    for directory in snapshot.additional_directories {outln!(console, "Additional directory: {}",safe_text(&directory.to_string_lossy()));}
                                    show_settings_warnings(console, agent.user_settings_warnings(chat))?;
                                },
                                Err(error) => outln!(console, "Cannot update directories: {}",safe_text(&error.to_string())),
                            }
                        },
                        _ if line.starts_with("/acknowledge-create ") || line.starts_with("/acknowledge-input ") => {
                            let (command,argument)=line.split_once(' ').unwrap_or((line.as_str(),""));
                            let Ok(id)=argument.trim().parse::<uuid::Uuid>() else {
                                outln!(console,"Usage: {command} COMMAND_ID");
                                continue;
                            };
                            let message=if command=="/acknowledge-input" {
                                if agent.acknowledge_input(id) {format!("Input {id} is no longer tracked; nothing was resent")} else {format!("No input {id} with an unknown acceptance result")}
                            } else {
                                match agent.acknowledge_first_create(id) {
                                    Some(CreateAcknowledgement::DraftRestored)=>format!("Creation {id} is no longer tracked; nothing was resent. If it created an Agent, that Agent appears in Agents; the next input creates a new Agent"),
                                    Some(CreateAcknowledgement::ChoicesCancelled)=>format!("Choices not yet applied after {id} are cancelled; nothing was resent. The Agent keeps the choices already applied and now takes input"),
                                    None=>format!("No unknown or failed New Agent command {id}"),
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
                        _ if is_task_surface_command(&line) || line.starts_with("/background ") || line.starts_with("/stop-command ") || line.starts_with("/command-output ") => {
                            if let Some(chat)=selected_chat {
                                if let Err(error)=command_control(console, agent,chat,&line).await {outln!(console, "Command control failed: {}",safe_text(&error));}
                            } else {outln!(console, "No Chat is selected.");}
                        },
                        "/operations" => {
                            if let Some(chat)=selected_chat {
                                let snapshot=agent.snapshot(chat).await.map_err(|error|error.to_string())?;
                                outln!(console, "Operations recorded in this process: {}",snapshot.operations.len());
                                for operation in snapshot.operations {
                                    outln!(console, "Operation: {} · {:?} · goal {}\n{} · {}",operation.id,operation.state,operation.loop_id,safe_text(&operation.name),safe_text(&operation.input.to_string()));
                                    if let Some(result)=operation.result {outln!(console, "Result: {}",safe_text(&result.content));}
                                }
                            } else {outln!(console, "No Chat is selected.");}
                        },
                        _ if line=="/clear-trigger" || line.starts_with("/events ") || line.starts_with("/trigger ") => {
                            if let (Some(chat),Some(selected_goal))=(selected_chat,goal) {
                                match trigger_control(console, agent,chat,selected_goal,&line).await {
                                    Ok(Some(receipt))=>{outln!(console, "Accepted: Goal {} · Run {}",receipt.loop_id,receipt.run_id);pending.insert(receipt.run_id,receipt);},
                                    Ok(None)=>{},
                                    Err(error)=>outln!(console, "Trigger control failed: {}",safe_text(&error)),
                                }
                            } else {outln!(console, "Select a Chat and goal first; use /loops.");}
                        },
                        "/loops" => {
                            if let Some(chat) = selected_chat {
                                let snapshot = agent.snapshot(chat).await.map_err(|error|error.to_string())?;
                                for goal in snapshot.loops { outln!(console, "Goal: {} · {:?} · {}",goal.id,goal.state,safe_text(&goal.goal)); }
                            } else { outln!(console, "No Chat is selected."); }
                        },
                        command if is_subscription_view_command(command) => {
                            if let Some(chat) = selected_chat {
                                for row in agent.tick_subscriptions(chat).await.map_err(|error|error.to_string())? {
                                    outln!(console, "Tick Subscription: {} · Tick Instance {} · every {}s · {} new · {} · {:?}",row.id,row.instance,row.every_seconds,row.accumulated,subscription_surface::last_delivery(&row),row.link);
                                }
                            } else { outln!(console, "No Chat is selected."); }
                        },
                        _ if line.starts_with("/plan ") => {
                            if let Some(chat) = selected_chat {
                                let snapshot = agent.snapshot(chat).await.map_err(|error|error.to_string())?;
                                let selected_goal = if let Some(value)=line.strip_prefix("/plan ") {value.trim().parse().ok()} else {goal};
                                if let Some(plan)=snapshot.plans.iter().find(|plan|Some(plan.loop_id)==selected_goal) {
                                    outln!(console, "Plan: {} · version {} · approved mode {:?}\n{}",plan.loop_id,plan.version,plan.approved_execution_mode,safe_text(&plan.markdown));
                                } else {outln!(console, "The selected goal has no saved plan.");}
                            } else { outln!(console, "No Chat is selected."); }
                        },
                        _ if line.starts_with("/approve-plan ") => {
                            if let Some(chat) = selected_chat {
                                let result = async {
                                    let fields=line.split_whitespace().collect::<Vec<_>>();
                                    if fields.len()!=4 {return Err("use /approve-plan GOAL VERSION MODE".to_string());}
                                    let selected_goal=fields[1].parse().map_err(|_|"goal ID must be a UUID")?;
                                    let version=fields[2].parse().map_err(|_|"plan version must be a number")?;
                                    let mode=serde_json::from_value::<ExecutionMode>(serde_json::Value::String(fields[3].into())).map_err(|_|"invalid execution mode")?;
                                    agent.approve_plan(chat,selected_goal,version,mode).await.map_err(|error|error.to_string())?;
                                    Ok::<_,String>((selected_goal,version,mode))
                                }.await;
                                match result {
                                    Ok((selected_goal,version,mode))=>{goal=Some(selected_goal);outln!(console, "Plan version {version} approved for goal {selected_goal} with mode {mode}. Enter a message to begin implementation.");},
                                    Err(error)=>outln!(console, "Cannot approve plan: {}",safe_text(&error)),
                                }
                            } else { outln!(console, "No Chat is selected."); }
                        },
                        "/interrupt" => {
                            if let Some(chat) = selected_chat {
                                let operations = agent.interrupt(chat).await.map_err(|error|error.to_string())?;
                                outln!(console, "Interrupt requested for the current run. The goal can continue with a new input.");
                                for operation in operations { outln!(console, "Operation {} remains {:?}: {}",operation.id,operation.state,safe_text(&operation.name)); }
                            } else { outln!(console, "No Chat is selected."); }
                        },
                        _ if line == "/stop" || line.starts_with("/stop ") => {
                            if let Some(chat) = selected_chat {
                                let selected_goal = if let Some(value) = line.strip_prefix("/stop ") { value.trim().parse().ok() } else { goal };
                                if let Some(selected_goal) = selected_goal {
                                    match agent.stop(chat, selected_goal).await {
                                        Ok(operations) => {
                                            let runs = pending.values().filter(|receipt|receipt.loop_id==selected_goal).map(|receipt|receipt.run_id).collect::<Vec<_>>();
                                            pending.retain(|_,receipt|receipt.loop_id!=selected_goal);
                                            permissions.retain(|_,run| !runs.contains(run));
                                            questions.retain(|_,question|question.chat_id!=chat||question.loop_id!=selected_goal);
                                            outln!(console, "Stopped goal {selected_goal}. Future inputs require another goal; use /loops new.");
                                            for operation in operations { outln!(console, "Operation {} remains {:?}: {}",operation.id,operation.state,safe_text(&operation.name)); }
                                        },
                                        Err(error) => outln!(console, "Cannot stop goal: {}",safe_text(&error.to_string())),
                                    }
                                } else { outln!(console, "Use /stop GOAL, or select a goal first."); }
                            } else { outln!(console, "No Chat is selected."); }
                        },
                        _ if line.split_whitespace().next() == Some("/loops") => {
                            match loops::LoopCommand::parse(&line) {
                                Ok(loops::LoopCommand::New) => {
                                    goal=None;
                                    outln!(console, "The next input will start a new goal in this Chat.");
                                },
                                Ok(action) => {
                                    if let Some(chat)=selected_chat {
                                        let result = async {
                                            match action {
                                                loops::LoopCommand::Select(id) => {
                                                    let snapshot=agent.snapshot(chat).await.map_err(|error|error.to_string())?;
                                                    if !snapshot.loops.iter().any(|item|item.id==id) {return Err("Goal does not belong to the current Chat; use /loops.".to_string());}
                                                    goal=Some(id);
                                                    outln!(console,"Selected goal {id}.");
                                                },
                                                loops::LoopCommand::Trigger(id) => {
                                                    let id=id.or(goal).ok_or("Select a goal first; use /loops.")?;
                                                    let snapshot=agent.snapshot(chat).await.map_err(|error|error.to_string())?;
                                                    let selected=snapshot.loops.iter().find(|item|item.id==id).ok_or("Goal does not belong to the current Chat; use /loops.")?;
                                                    outln!(console,"Goal: {id}\n{}",loops::trigger_lines(selected.trigger.as_ref()).join("\n"));
                                                },
                                                loops::LoopCommand::Schedule {goal, arguments} => {
                                                    let trigger=loops::TimerSchedule::parse(arguments)?.apply(agent,chat,goal).await?;
                                                    print_trigger(console,&trigger)?;
                                                },
                                                _ => unreachable!(),
                                            }
                                            Ok::<_,String>(())
                                        }.await;
                                        if let Err(error)=result {outln!(console,"Cannot manage Loop: {}",safe_text(&error));}
                                    } else {outln!(console,"No Chat is selected.");}
                                },
                                Err(error) => outln!(console,"{error}"),
                            }
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
                        _ if line.starts_with("/answer ") => {
                            let request=line.trim_start_matches("/answer ").split_once(' ').ok_or("use /answer QUESTION ANSWER").and_then(|(id,answer)|{
                                let id=id.parse::<uuid::Uuid>().map_err(|_|"question ID must be a UUID")?;
                                let request=questions.get(&id).cloned().ok_or("question expired")?;
                                Ok((request,answer.to_owned()))
                            });
                            match request {
                                Ok((request,answer)) => {
                                    if answer_typed_question(agent,console,&mut menus,&mut questions,&request,&answer).await? && draft_question==Some(request.id) {draft_question=None;}
                                },
                                Err(error) => outln!(console, "Cannot answer question: {}",safe_text(error)),
                            }
                        },
                        _ if line.starts_with("/resume ") => {
                            match open_chat(agent, &line, selected_chat).await {
                                Ok(snapshot) => {
                                    goal=show_opened_chat(console,agent,&snapshot,true)?;
                                    selected_chat=Some(snapshot.id);
                                    draft_question=None;
                                    task_subscription=None;
                                    message_groups=MessageGroups::default();
                                    displayed_sequences.insert(snapshot.id,snapshot.last_sequence);
                                    tools.restore(&snapshot.conversation_events());
                                    sync_snapshot(&snapshot,&mut pending,&mut permissions,&mut questions);
                                    transcript_dirty=true;
                                },
                                Err(error) => outln!(console, "{}", safe_text(&error)),
                            }
                        },
                        _ if line.starts_with('/') => {
                            let command = line.trim_start_matches('/');
                            let (name, arguments) = command
                                .split_once(char::is_whitespace)
                                .map_or((command, ""), |(name, arguments)| (name, arguments.trim()));
                            let directory=global.projection().dispatch_directory;
                            if selected_chat.is_none() && agent.check_new_agent_skill(&directory,name,arguments).is_err() {
                                outln!(console, "Unknown command; use /help.");
                                console.restore_rejected_prompt(restored_contexts,"Unknown command; use /help")?;
                                continue;
                            }
                            let chat_id = match ensure_chat_for_input(console,agent,&mut selected_chat,&directory,retry_of,&original_input,&mut echo).await? {
                                Ok(chat)=>chat,
                                Err(error)=>{console.restore_rejected_prompt(restored_contexts,&error)?;status_dirty=true;continue;},
                            };
                            if !agent.has_skill(chat_id, name).await.map_err(|error|error.to_string())? {
                                outln!(console, "Unknown command; use /help.");
                                continue;
                            }
                            let sent=send_input(agent,chat_id,retry_of,agent.invoke_skill(chat_id, goal, name, arguments),submitted_receipt).await;
                            match sent {
                                InputSend::Accepted { value: receipt, adopted } => {
                                    echo=track_accepted_echo(console,agent,&receipt,echo).await?;
                                    merged_echoes.accepted(receipt.run_id,echo);
                                    goal = Some(receipt.loop_id);
                                    if !interactive {outln!(console, "Accepted: Goal {} · Run {}", receipt.loop_id, receipt.run_id);}
                                    track_accepted_input(console,agent,receipt,adopted,&mut pending,&mut permissions,&mut questions).await?;
                                },
                                InputSend::Unknown(id, error) => {
                                    if let Some(echo)=echo {console.withdraw_echo(Some(chat_id),echo)?;}
                                    outln!(console, "Failed: {}", safe_text(&error.to_string()));
                                    console.restore_unknown_prompt(restored_contexts,&safe_line(&error.to_string()),Some(id))?;
                                },
                                InputSend::Held(error) => {
                                    if retry_of.is_none() && let Some(echo)=echo {console.withdraw_echo(Some(chat_id),echo)?;}
                                    outln!(console, "Failed: {}", safe_text(&error.to_string()));
                                    console.restore_unknown_prompt(restored_contexts,&safe_line(&error.to_string()),None)?;
                                },
                                InputSend::Failed(error) => outln!(console, "Unknown or unavailable Skill: {}", safe_text(&error.to_string())),
                            }
                        },
                        _ => {
                            if let Some(request)=draft_question.and_then(|id|questions.get(&id)).filter(|request|Some(request.chat_id)==selected_chat).cloned() {
                                if answer_typed_question(agent,console,&mut menus,&mut questions,&request,&original_input).await? {draft_question=None;}
                                continue;
                            }
                            let visible=questions.values().filter(|request|Some(request.chat_id)==selected_chat).cloned().collect::<Vec<_>>();
                            if !visible.is_empty() {
                                if let [request]=visible.as_slice() {
                                    answer_typed_question(agent,console,&mut menus,&mut questions,request,&original_input).await?;
                                } else {outln!(console, "More than one question is pending; use /answer QUESTION ANSWER.");}
                                continue;
                            }
                            let directory=global.projection().dispatch_directory;
                            let chat_id = match ensure_chat_for_input(console,agent,&mut selected_chat,&directory,retry_of,&original_input,&mut echo).await? {
                                Ok(chat)=>chat,
                                Err(error)=>{console.restore_rejected_prompt(restored_contexts,&error)?;status_dirty=true;continue;},
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
                                goal,
                                original_input,
                                contexts,
                                file_references,
                            );
                            match send_input(agent,chat_id,retry_of,submission,submitted_receipt).await {
                                InputSend::Accepted { value: receipt, adopted } => {
                                    echo=track_accepted_echo(console,agent,&receipt,echo).await?;
                                    merged_echoes.accepted(receipt.run_id,echo);
                                    goal = Some(receipt.loop_id);
                                    if !interactive {outln!(console, "Accepted: Goal {} · Run {}", receipt.loop_id, receipt.run_id);}
                                    track_accepted_input(console,agent,receipt,adopted,&mut pending,&mut permissions,&mut questions).await?;
                                },
                                InputSend::Failed(error @ AgentError::ImageInputUnsupported) => {
                                    if retry_of.is_none() && let Some(echo)=echo {console.withdraw_echo(Some(chat_id),echo)?;}
                                    outln!(console, "Failed: {}", safe_text(&error.to_string()));
                                    console.restore_rejected_prompt(submitted_contexts, "Prompt kept for editing")?;
                                    failed=true;
                                },
                                InputSend::Unknown(id, error) => {
                                    if let Some(echo)=echo {console.withdraw_echo(Some(chat_id),echo)?;}
                                    outln!(console, "Failed: {}", safe_text(&error.to_string()));
                                    console.restore_unknown_prompt(submitted_contexts,&safe_line(&error.to_string()),Some(id))?;
                                    failed=true;
                                },
                                InputSend::Held(error) => {
                                    if retry_of.is_none() && let Some(echo)=echo {console.withdraw_echo(Some(chat_id),echo)?;}
                                    outln!(console, "Failed: {}", safe_text(&error.to_string()));
                                    console.restore_unknown_prompt(submitted_contexts,&safe_line(&error.to_string()),None)?;
                                    failed=true;
                                },
                                InputSend::Failed(error) => {outln!(console, "Failed: {}", safe_text(&error.to_string())); failed=true;},
                            }
                        },
                    }
                }
                Some(InputEvent::CycleExecutionMode) => {
                    let target=ConfigTarget::visible(selected_chat);
                    // The overview shows no Agent; `selected_chat` there is the hidden one.
                    let hidden=overview_active.then(||completion::unavailable_command("/mode",completion::CommandScope::Overview)).flatten();
                    let result=if let Some((_,reason))=hidden {Err(format!("Shift+Tab mode is unavailable here: {reason}"))} else {match agent.configuration(target).await {
                        Ok(configuration)=>{
                            let change=ConfigurationChange::ExecutionMode {mode:configuration.execution_mode.next()};
                            match configuration.unavailable_change(&change) {
                                Some(reason)=>Err(completion::command_refusal(if configuration.delegated {completion::CommandScope::Child} else {completion::CommandScope::Root},"Shift+Tab mode",reason)),
                                None=>agent.change_configuration(target,change).await.map(|_|()).map_err(|error|format!("Cannot change execution mode: {}",safe_text(&error.to_string()))),
                            }
                        },
                        Err(error)=>Err(format!("Cannot read execution mode: {}",safe_text(&error.to_string()))),
                    }};
                    if let Err(message)=result {console.hold_status(&message)?;}
                    status_dirty=true;
                }
            }},
            event = async {
                if !visual_pacing_pending && let Some(event) = deferred_events.pop_front() {
                    Ok(event)
                } else {
                    events.recv().await
                }
            } => {
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
                        if matches!(event.kind,EventKind::Started) && event.sequence>*displayed_sequences.get(&event.chat_id).unwrap_or(&0) {
                            pending.entry(event.run_id).or_insert(Receipt {chat_id:event.chat_id,loop_id:event.loop_id,run_id:event.run_id});
                            observed_run_starts.entry(event.run_id).or_insert_with(std::time::Instant::now);
                            #[cfg(test)]
                            agent.record_recovery_fact(recovery_tests::RecoveryFact::LiveRunStarted {chat:event.chat_id,run:event.run_id,overview:overview_active});
                        }
                        if matches!(event.kind,EventKind::Started|EventKind::InputMerged {..}) {
                            let echo=show_consumed_input(console,agent,event).await?;
                            merged_echoes.consumed_echo(event.run_id,echo);
                        }
                        if let Some((previous,current))=merged_echoes.observe(event,displayed_sequences.get(&event.chat_id).copied().unwrap_or(0)) {console.join_inputs(event.chat_id,previous,current)?;}
                        #[cfg(test)]
                        if let EventKind::InputMerged { into_run_id } = &event.kind {
                            agent.record_recovery_fact(recovery_tests::RecoveryFact::MergeObserved {chat:event.chat_id,run:event.run_id,sequence:event.sequence,into_run:*into_run_id});
                        }
                    },
                    Err(_) => merged_echoes.close_all(),
                }
                match event {
                Ok(event) if !overview_active && (pending.contains_key(&event.run_id) || Some(event.chat_id)==selected_chat) => {
                    if Some(event.chat_id)==selected_chat && matches!(event.kind,EventKind::Started|EventKind::InputMerged {..})
                        && let Ok(snapshot)=agent.snapshot(event.chat_id).await {
                        show_consumed_agent_message(console,&snapshot,event.run_id)?;
                    }
                    transcript_dirty |= Some(event.chat_id)==selected_chat;
                    if event.sequence<=*displayed_sequences.get(&event.chat_id).unwrap_or(&0) {
                        #[cfg(test)]
                        agent.record_recovery_fact(recovery_tests::RecoveryFact::Skipped {
                            chat: event.chat_id,
                            run: event.run_id,
                            sequence: event.sequence,
                            completed: matches!(event.kind, EventKind::Completed { .. }),
                        });
                        continue;
                    }
                    let was_thinking = thinking_runs.contains(&event.run_id);
                    match &event.kind {
                        EventKind::Thinking(thinking) => {
                            thinking_runs.insert(event.run_id);
                            if interactive && Some(event.chat_id) == selected_chat && !unpaced_runs.contains(&event.run_id) {
                                let preview = thinking_previews.entry(event.run_id).or_default();
                                preview.append(thinking);
                                activity_dirty = true;
                            }
                        },
                        EventKind::Text(_)
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
                        thinking_previews.remove(&event.run_id);
                    }
                    status_dirty |= !matches!(&event.kind, EventKind::Text(_) | EventKind::Thinking(_)) || stopped_thinking;
                    track_run_requests(&event,&mut pending,&mut permissions,&mut questions);
                    if matches!(&event.kind,EventKind::Failed(_)) {
                        failed=true;
                        if let Ok(snapshot) = agent.snapshot(event.chat_id).await {
                            menus.invalidate_failed_directory(&snapshot, event.run_id);
                        }
                    }
                    if matches!(&event.kind,EventKind::Started) {
                        working_shimmer.reset();
                    }
                    if matches!(&event.kind, EventKind::ConversationRewound { .. }) {
                        if Some(event.chat_id) != selected_chat {
                            continue;
                        }
                        let snapshot = agent
                            .snapshot(event.chat_id)
                            .await
                            .map_err(|error| error.to_string())?;
                        displayed_sequences.insert(event.chat_id, snapshot.last_sequence);
                        tools.restore(&snapshot.conversation_events());
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
                        console.reset_chat(event.chat_id)?;
                        workflows::write_public_history(console, &snapshot)?;
                        outln!(
                            console,
                            "Conversation restored. Ctrl+O opens the full archive."
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
                            .is_some_and(|id| displayed_assistant_messages.insert(id.clone()));
                    let starts_thinking = matches!(&event.kind, EventKind::Thinking(_))
                        && message_id
                            .as_ref()
                            .is_some_and(|id| displayed_assistant_messages.insert(id.clone()));
                    let follows_thinking = starts_assistant_message
                        && message_id.as_ref().is_some_and(|id| {
                            displayed_assistant_messages
                                .contains(&id.replace(":assistant:", ":thinking:"))
                        });
                    displayed_sequences.insert(event.chat_id,event.sequence);
                    let console = &console.for_message(message_id.clone());
                    tools.record(&event);
                    if interactive && matches!(&event.kind,
                        EventKind::Thinking(_)
                        | EventKind::ToolStarted { .. }
                        | EventKind::PermissionRequested { .. }
                        | EventKind::QuestionAsked { .. }
                        | EventKind::ModelRetry { .. }
                        | EventKind::Completed { .. }
                        | EventKind::Failed(_)
                        | EventKind::Interrupted
                    )
                        && let Some(mut reply) = reply_lines.remove(&event.run_id) {
                            reply.finish();
                            write_next_reply_line(console, &mut reply)?;
                    }
                    if interactive && stopped_thinking {
                        console.write_roles(&[("\n∴ Thinking (ctrl+o to expand)\n".into(), Some(Role::MutedText))])?;
                    }
                    match &event.kind {
                        EventKind::ToolCompleted {call_id, ..} => console.finish_tool_call(Some(call_id))?,
                        EventKind::Completed {..} | EventKind::Failed(_) | EventKind::Interrupted => {
                            console.finish_tool_call(None)?;
                            unpaced_runs.remove(&event.run_id);
                        },
                        _ => {},
                    }
                    if let Some(rendered)=tools.render_event_roles(&event, console.display_profile()?) {
                        match &event.kind {
                            EventKind::ToolStarted {call_id, ..} if interactive && !tools.is_detailed() => console.write_tool_call(call_id, &rendered)?,
                            _ => console.write_roles(&rendered)?,
                        }
                        continue;
                    }
                    match event.kind {
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
                                let reply = reply_lines.entry(event.run_id).or_insert_with(|| ReplyLines::new(event.chat_id, message_id));
                                reply.push(&safe_text(&text));
                                #[cfg(test)]
                                agent.record_recovery_fact(recovery_tests::RecoveryFact::Buffered {
                                    chat: event.chat_id,
                                    run: event.run_id,
                                    sequence: event.sequence,
                                    complete_line: reply.has_complete_line(),
                                });
                                if unpaced_runs.contains(&event.run_id) {
                                    write_pending_reply_lines(console, reply)?;
                                } else {
                                    write_next_reply_line(console, reply)?;
                                }
                                #[cfg(test)]
                                agent.recovery_checkpoint().await;
                            } else {
                                if follows_thinking {console.write("\nAssistant: ")?;}
                                console.write_markdown(&safe_text(&text), starts_assistant_message)?;
                            }
                        },
                        EventKind::Completed {usage} => {
                            #[cfg(test)]
                            agent.record_recovery_fact(recovery_tests::RecoveryFact::Completed {chat:event.chat_id,run:event.run_id,sequence:event.sequence});
                            write_completion_notice(console, &usage)?;
                        },
                        EventKind::Failed(error) => {
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
                            let message = format!(
                                "{}\n",
                                model_retry_notice(kind, &error, attempt, max_retries, delay_ms),
                            );
                            if interactive {
                                console.write_roles(&[(message, Some(Role::Warning))])?;
                            } else {
                                console.write(&message)?;
                            }
                        },
                        EventKind::Interrupted => {
                            let snapshot=agent.snapshot(event.chat_id).await.map_err(|error|error.to_string())?;
                            let stopped=snapshot.loops.iter().any(|goal|goal.id==event.loop_id&&goal.state==tickloop_agent::runtime::LoopState::Stopped);
                            if interactive && !stopped {
                                console.write_roles(&[(format!("\n{}\n", presentation::interrupted_notice(console.display_profile()?)), Some(Role::MutedText))])?;
                            } else {
                                outln!(console, "\n{} goal {} · run {}",if stopped {"Stopped"} else {"Interrupted"},event.loop_id,event.run_id);
                            }
                        },
                        EventKind::InputMerged { into_run_id } => {
                            if !interactive {
                                outln!(console, "Queued input joined run {into_run_id}.");
                            }
                        },
                        EventKind::InputTakenBack => {},
                        EventKind::ConversationRewound { .. } => unreachable!(),
                        EventKind::GoalEnded {reason,operations} => print_goal_end(console, event.loop_id,&reason,&operations)?,
                        EventKind::Started => {
                            pending.entry(event.run_id).or_insert(Receipt {chat_id:event.chat_id,loop_id:event.loop_id,run_id:event.run_id});
                            if interactive {
                                console.hide_completion_notice()?;
                            } else {
                                let snapshot = agent.snapshot(event.chat_id).await.map_err(|error|error.to_string())?;
                                outln!(console, "Goal: {} · Run: {} · Running\nModel: {} · {:?}", event.loop_id, event.run_id,safe_text(&snapshot.model.model),snapshot.model.protocol);
                            }
                        },
                        EventKind::PermissionRequested {request} => {
                            if interactive {outln!(console,"\nWaiting for your confirmation to continue.");}
                            else if request.name==ENTER_PLAN_MODE_TOOL_NAME {outln!(console, "\nEnter plan mode? Request: {}\nThe Agent wants to enter plan mode to explore and design an implementation approach.\nUse /allow {} to enter plan mode or /deny {} to stay in the current mode.", request.id, request.id, request.id);}
                            else {outln!(console, "\nPermission request: {} · {}\nOperation: {}\nUse /allow {} [once|session|persistent] or /deny {}.", request.id, safe_text(&request.name), safe_text(&request.input.to_string()), request.id, request.id);}

                        },
                        EventKind::ApprovalAssessed { .. } => {},
                        EventKind::AutoReviewerNotice { notice } => {
                            let message = match notice {
                                tickloop_agent::runtime::AutoReviewerNotice::Fallback { failure } => format!("Jev review unavailable ({failure:?}); using this Chat's LLM for Auto review"),
                                tickloop_agent::runtime::AutoReviewerNotice::Recovered => "Jev Auto review recovered".into(),
                            };
                            outln!(console, "\n{}", safe_text(&message));
                        },
                        EventKind::PermissionChecked {decision,reason} => {
                            if !interactive || decision != Decision::Deny {
                                outln!(console, "\nAuto review: {decision:?} · {}",safe_text(&reason));
                            }
                        },
                        EventKind::QuestionAsked {request} => print_question(console, &request)?,
                        EventKind::PlanUpdated {plan} => {
                            outln!(console, "\nPlan: {} · version {}\n{}",plan.loop_id,plan.version,safe_text(&plan.markdown));
                            let snapshot=agent.snapshot(event.chat_id).await.map_err(|error|error.to_string())?;
                            if snapshot.policy.mode==ExecutionMode::Plan {
                                if interactive {outln!(console,"Use /plan to review and choose how to start, or enter revisions.");}
                                else {outln!(console, "Review or request revisions. To approve: /approve-plan {} {} MODE",plan.loop_id,plan.version);}
                            }
                        },
                        EventKind::ProgressUpdated {progress} => {
                            outln!(console, "\nProgress for goal {}:",progress.loop_id);
                            for todo in progress.todos {outln!(console, "  {:?}: {}",todo.status,safe_text(&todo.content));}
                        },
                        EventKind::CommandUpdated {..} => {},
                        EventKind::ContextCompaction {status,error} => {
                            outln!(console, "\nContext compaction: {status:?}");
                            if let Some(error)=error {outln!(console, "{}",safe_text(&error));}
                        },
                        EventKind::ToolStarted {..} | EventKind::ToolCompleted {..} | EventKind::AgentMessageQueued {..} => {},
                    }

                },
                Ok(event) => {
                    track_run_requests(&event,&mut pending,&mut permissions,&mut questions);
                    status_dirty |= announce_background_event(console,&event,&background_label(agent,console,event.chat_id),&mut background_notice)?;
                },
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    #[cfg(test)]
                    agent.record_recovery_fact(recovery_tests::RecoveryFact::Lagged);
                    thinking_previews.clear();
                    deferred_events.clear();
                    transcript_dirty = true;
                    status_dirty = true;
                    if !overview_active && let Some(chat)=selected_chat {
                        let frame_hold=console.hold_frames()?;
                        outln!(console, "The view fell behind; restoring it from saved execution events.");
                        let observation=restore_view(console, agent,&mut tools,chat,*displayed_sequences.get(&chat).unwrap_or(&0),&mut reply_lines).await?;
                        drop(frame_hold);
                        failed |= observation.snapshot.loops.iter().any(|goal|goal.state==tickloop_agent::runtime::LoopState::Failed);
                        displayed_sequences.insert(chat,observation.snapshot.last_sequence);
                        sync_snapshot(&observation.snapshot,&mut pending,&mut permissions,&mut questions);
                    }
                    let mut background_snapshots=agent.list_chats().await.into_iter().filter(|snapshot|overview_active || Some(snapshot.id)!=selected_chat).collect::<Vec<_>>();
                    let unlisted_chats=pending.values().map(|receipt|receipt.chat_id)
                        .filter(|chat|(overview_active || Some(*chat)!=selected_chat)&&!background_snapshots.iter().any(|snapshot|snapshot.id==*chat))
                        .collect::<HashSet<_>>();
                    for chat in unlisted_chats {
                        match agent.snapshot(chat).await {
                            Ok(snapshot)=>background_snapshots.push(snapshot),
                            Err(AgentError::NotFound)=>{
                                let runs=pending.extract_if(|_,receipt|receipt.chat_id==chat).map(|(run,_)|run).collect::<HashSet<_>>();
                                permissions.retain(|_,run|!runs.contains(run));
                                questions.retain(|_,question|question.chat_id!=chat);
                            },
                            Err(error)=>return Err(error.to_string()),
                        }
                    }
                    for snapshot in &background_snapshots {
                        let recovery=recover_background_chat(snapshot,&background_label(agent,console,snapshot.id),&mut pending,&mut permissions,&mut questions);
                        failed |= recovery.failed;
                        if !interactive {
                            for notice in &recovery.run_notices {outln!(console,"Background {}",notice.text(&background_label(agent,console,snapshot.id)));}
                            for notice in &recovery.request_notices {outln!(console,"Background {notice}");}
                        }
                        if let Some(notice)=recovery.run_notices.last() {background_notice=Some((snapshot.id,notice.clone()));}
                    }
                },
                Err(broadcast::error::RecvError::Closed) => return Err("agent event stream closed".into()),
                }
            },
            event = user_shell_events.recv() => match event {
                Ok(record) if user_shell_offsets.contains_key(&record.id) => {
                    status_dirty=true;
                    let observation=observe_user_shell_record(&mut foreground_shells,&mut backgrounded_shells,&record);
                    let terminal=user_shell_is_terminal(&record);
                    if !overview_active && Some(record.chat_id)==selected_chat {
                        if let Some(foreground)=observation.foreground {console.set_foreground_shell(foreground)?;}
                        if observation.render_in_chat && render_user_shell_update(agent,console,&record,&mut user_shell_offsets).await? {
                            user_shell_chats.remove(&record.id);
                            transcript_dirty=true;
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
                        let snapshot=agent.snapshot(chat).await.map_err(|error|error.to_string())?;
                        let tracked_records=snapshot.user_shells.into_iter().filter(|record|user_shell_offsets.contains_key(&record.id)).collect::<Vec<_>>();
                        for record in tracked_records {
                            let observation=observe_user_shell_record(&mut foreground_shells,&mut backgrounded_shells,&record);
                            let terminal=user_shell_is_terminal(&record);
                            if !overview_active && Some(record.chat_id)==selected_chat {
                                if let Some(foreground)=observation.foreground {console.set_foreground_shell(foreground)?;}
                                if observation.render_in_chat && render_user_shell_update(agent,console,&record,&mut user_shell_offsets).await? {
                                    user_shell_chats.remove(&record.id);
                                    transcript_dirty=true;
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
                    Ok((_,(chat,Ok(outcome))))=>outln!(console, "Context compacted for Chat {chat}. Earlier messages stay in the full archive and are no longer sent to the model.\n{}",safe_text(&outcome.summary)),
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
                            displayed_sequences.insert(metadata.chat,effect.snapshot.last_sequence);
                            merged_echoes.close_all();
                            tools.restore(&effect.snapshot.conversation_events());
                            message_groups=MessageGroups::default();
                            sync_snapshot(&effect.snapshot,&mut pending,&mut permissions,&mut questions);
                            selected_chat=Some(metadata.chat);
                            if effect.clear_goal {goal=None;}
                            transcript_dirty=true;
                        },
                        Err(error) => {
                            let effect=directed_summary::finish_failure(console,&error)?;
                            console.finish_summary_input_transition()?;
                            failed |= effect.failed;
                            transcript_dirty=true;
                        },
                    },
                    SummaryTaskCompletion::ObserverStopped {metadata}=>{
                        summary_recovery_required.insert(metadata.chat);
                        outln!(console,"Conversation summary observer for Chat {} stopped before receiving its result. Restart the Agent host to recover before continuing.",metadata.chat);
                        console.finish_summary_input_transition()?;
                        failed=true;
                    },
                }
                if directed_summaries.is_empty() && summary_recovery_required.is_empty() && login.is_none() {
                    console.set_prompt_enabled(true)?;
                }
            },
            _ = global_refresh.tick(), if global_refresh_enabled => {
                match global.refresh().await {
                    Ok(_)=>{
                        if let Ok(Some(receipt))=global.reconcile_dispatch().await {
                            match agent.accept_created(receipt).await {
                                CreationAcceptance::Attached(submitted)=>{if let Some(receipt)=submitted {pending.insert(receipt.run_id,receipt);}},
                                CreationAcceptance::AttachUnavailable(error)=>console.set_status(&creation_attach_unavailable(&error))?,
                                CreationAcceptance::Rejected(error)=>console.set_status(&safe_text(&format!("Creation receipt resolved: {error}")))?,
                            }
                        }
                        let projection=global.projection();
                        // A run whose execution the owner has lost can no longer report its end.
                        for notice in end_runs_of_lost_executions(&projection.rows,&mut pending,&mut permissions,&mut questions) {
                            failed=true;
                            if interactive {
                                let label=readable_background_label(agent,console,notice.chat);
                                console.record_execution_loss(notice.chat,console::ExecutionLossNotice {
                                    run_id:notice.run,
                                    summary:format!("\n{label} · Execution connection was lost · outcome unknown for that attempt; nothing was resent. Use /resume to open saved history; continuing requires an explicit new input.\n"),
                                    technical:notice.text,
                                })?;
                                transcript_dirty |= Some(notice.chat)==selected_chat;
                            } else {outln!(console,"{}",notice.text);}
                            if Some(notice.chat)!=selected_chat {background_notice=Some((notice.chat,BackgroundRunNotice { message: "Outcome unknown · execution lost · nothing resent".into(), compact_message: Some("Unknown/lost/no resend".into()), necessary: true }));}
                        }
                        console.set_creation_notice(creation_notice(&projection))?;
                        if overview_active {console.set_global_overview(Some(agent_overview(&projection)))?;}
                        let visible_rows=projection.visible.map(|execution|global.family(execution)).unwrap_or_default();
                        if let Some(chat)=selected_chat {
                            if let Some(row)=visible_rows.iter().find(|row|row.chat_id==chat) {
                                global.visible_ready(Some(row.execution));
                                let family=global.family(row.execution);
                                visible_family=family.iter().filter(|row|row.availability==crate::agent_access::ExecutionAvailability::Live).map(|row|row.chat_id).collect();
                                console.set_agent_family(if family.len()>1 {agent_tree_rows(&family,false)}else{Vec::new()},Some(row.execution.agent_id))?;
                                for row in family.into_iter().filter(|row|row.availability==crate::agent_access::ExecutionAvailability::Live) {
                                    if let Ok(snapshot)=agent.snapshot(row.chat_id).await {
                                        let awaiting_events=pending.values().filter(|receipt|receipt.chat_id==snapshot.id).cloned().collect::<Vec<_>>();
                                        let outcome_pending=awaiting_events.iter().any(|receipt|snapshot.loops.iter().any(|goal|goal.id==receipt.loop_id && matches!(goal.state,tickloop_agent::runtime::LoopState::Completed|tickloop_agent::runtime::LoopState::Failed|tickloop_agent::runtime::LoopState::Interrupted|tickloop_agent::runtime::LoopState::Stopped)));
                                        // Events already queued for this View finish the run themselves; restoring
                                        // from the snapshot now would print the reply a second way.
                                        if outcome_pending && !events.is_empty() && Some(snapshot.id)==selected_chat {
                                            continue;
                                        }
                                        if outcome_pending {
                                            if !overview_active && Some(snapshot.id)==selected_chat {
                                                let restored=restore_view(console,agent,&mut tools,snapshot.id,*displayed_sequences.get(&snapshot.id).unwrap_or(&0),&mut reply_lines).await?;
                                                failed |= restored.snapshot.loops.iter().any(|goal|goal.state==tickloop_agent::runtime::LoopState::Failed);
                                                displayed_sequences.insert(snapshot.id,restored.snapshot.last_sequence);
                                                merged_echoes.close_all();
                                                sync_snapshot(&restored.snapshot,&mut pending,&mut permissions,&mut questions);
                                            } else {
                                                let recovery=recover_background_chat(&snapshot,&background_label(agent,console,snapshot.id),&mut pending,&mut permissions,&mut questions);
                                                failed |= recovery.failed;
                                                for notice in recovery.run_notices {
                                                    if !interactive {outln!(console,"Background {}",notice.text(&background_label(agent,console,snapshot.id)));}
                                                    background_notice=Some((snapshot.id,notice));
                                                }
                                                for notice in recovery.request_notices {
                                                    if !interactive {outln!(console,"Background {notice}");}
                                                }
                                            }
                                            continue;
                                        }
                                        sync_snapshot(&snapshot,&mut pending,&mut permissions,&mut questions);
                                        for receipt in awaiting_events {pending.entry(receipt.run_id).or_insert(receipt);}
                                    }
                                }
                                if row.availability==crate::agent_access::ExecutionAvailability::Live {
                                    show_settings_warnings(console,agent.user_settings_warnings(chat))?;
                                }
                            }
                            else {visible_family.clear();console.set_agent_family(Vec::new(),None)?;}
                        } else {visible_family.clear();console.set_agent_family(Vec::new(),None)?;}
                        let background_request_chats=permissions.values()
                            .filter_map(|run|pending.get(run).map(|receipt|receipt.chat_id))
                            .chain(questions.values().map(|request|request.chat_id))
                            .filter(|chat|!visible_family.contains(chat))
                            .collect::<HashSet<_>>();
                        for chat in background_request_chats {
                            // A cached snapshot cannot prove that another View resolved a request.
                            if let Ok(execution)=agent.reference(chat).await
                                && let Ok(observation)=agent.access().observe(ObservationRequest::Agent {
                                    execution,detail:AgentDetailQuery::Snapshot,after:None,
                                }).await
                                && let ObservationValue::Agent(snapshot)=observation.initial
                            {
                                sync_snapshot_requests(&snapshot.conversation,&pending,&mut permissions,&mut questions);
                            }
                        }
                        let live_visible=selected_chat.filter(|chat|visible_rows.iter().any(|row|row.chat_id==*chat && row.availability==crate::agent_access::ExecutionAvailability::Live));
                        owner_unreachable=None;
                        let notice=if let Err(error)=agent.display_chat(if overview_active {None}else{live_visible}).await {
                            Some(format!("{VIEW_REGISTRATION_UNAVAILABLE_NOTICE} · {}",safe_line(&error.to_string())))
                        } else if let Err(error)=agent.renew_display().await {
                            Some(format!("View connection unavailable · {}",safe_line(&error.to_string())))
                        } else {None};
                        if notice.is_some() || selected_chat.is_none() || overview_active {
                            console.set_connection_notice(notice.or_else(||agent.disconnected()))?;
                        }
                        status_dirty=true;
                    }
                    Err(error)=>{
                        owner_unreachable=Some(format!("{DISCONNECTED_CACHED_VIEW_NOTICE} · {}",safe_text(&error.to_string())));
                        if !overview_active && selected_chat.is_some() {
                            status_dirty=true;
                        } else {console.set_connection_notice(owner_unreachable.clone())?;}
                    }
                }
            },
            _ = animation_refresh.tick(), if interactive && !prefers_reduced_motion && !visual_pacing_pending && (!pending.is_empty() || !directed_summaries.is_empty() || has_visible_running_task) => {
                activity_dirty = true;
            },
            _ = elapsed_refresh.tick(), if interactive && prefers_reduced_motion && !visual_pacing_pending && running_activity.as_ref().is_some_and(|activity| activity.started_at.is_some()) => {
                activity_dirty = true;
            },
            _ = tokio::task::yield_now(), if visual_pacing_pending => {
                if let Some(reply) = reply_lines.values_mut().find(|reply| Some(reply.chat_id) == selected_chat && reply.has_complete_line()) {
                    write_next_reply_line(console, reply)?;
                }
            },
            _ = subscription_refresh.tick(), if interactive && console.subscription_view_chat()?.is_some() => {
                if let Some(chat)=console.subscription_view_chat()? {
                    refresh_subscriptions(agent,console,chat).await?;
                }
            },
            _ = transcript_refresh.tick(), if transcript_dirty && console.transcript_id()?.is_some() => {
                if let (Some(chat), Some(id))=(selected_chat, console.transcript_id()?) {
                    let snapshot=agent.snapshot(chat).await.map_err(|error|error.to_string())?;
                    console.update_transcript(id,presentation::transcript_document(&snapshot, console.display_profile()?, &console.image_links()?))?;
                }
                transcript_dirty=false;
            },
            task_event = receive_shell_task_event(&mut task_subscription), if task_subscription.is_some() => {
                match task_event {
                    Some(Ok(ShellTaskSubscriptionEvent::Changed(_))) => {
                        let Some(chat)=task_subscription.as_ref().map(ShellTaskSubscription::chat_id) else {continue;};
                        match task_center.snapshot(chat).await {
                            Ok(snapshot) => {
                                let effect=console.update_tasks(snapshot)?;
                                if console.task_surface_chat()?==Some(chat) {
                                    apply_task_surface_effect(&task_center,console,chat,effect).await?;
                                }
                            }
                            Err(error) => {
                                console.task_updates_failed(chat,safe_text(&error.to_string()))?;
                                task_subscription=None;
                            }
                        }
                    }
                    Some(Ok(ShellTaskSubscriptionEvent::ResyncRequired { .. })) => {
                        if let Some(subscription)=task_subscription.as_mut() {
                            let chat=subscription.chat_id();
                            match subscription.resync().await {
                                Ok(snapshot) => {
                                    let effect=console.update_tasks(snapshot)?;
                                    if console.task_surface_chat()?==Some(chat) {
                                        apply_task_surface_effect(&task_center,console,chat,effect).await?;
                                    }
                                }
                                Err(error) => {
                                    console.task_updates_failed(chat,safe_text(&error.to_string()))?;
                                    task_subscription=None;
                                }
                            }
                        }
                    }
                    Some(Err(error)) => {
                        if let Some(subscription)=task_subscription.as_ref() {
                            console.task_updates_failed(subscription.chat_id(),safe_text(&error.to_string()))?;
                        }
                        task_subscription=None;
                    }
                    None => {}
                }
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
                    console.write_roles(&task_notification_line(
                        &notification,
                        console.display_profile()?,
                    ))?;
                    transcript_dirty=true;
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

async fn open_rewind(
    agent: &Agent,
    console: &Console,
    menus: &mut Menus,
    chat: Option<uuid::Uuid>,
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
        .snapshot(chat)
        .await
        .map_err(|error| error.to_string())?;
    if !snapshot.permissions.is_empty() || !snapshot.questions.is_empty() {
        let status = "Resolve the current permission or question before opening rewind";
        console.set_status(status)?;
        return Ok(Some(status.into()));
    }
    menus.rewind(console, &snapshot)
}

async fn open_tasks(
    task_center: &ShellTaskCenter,
    console: &Console,
    chat_id: uuid::Uuid,
    task_subscription: &mut Option<ShellTaskSubscription>,
    selected_task: Option<uuid::Uuid>,
    from_footer: bool,
) -> Result<(), String> {
    let prepared = async {
        let mut subscription = task_center.subscribe(chat_id).await?;
        let snapshot = subscription.resync().await?;
        Ok::<_, tickloop_agent::runtime::ShellTaskCenterError>((subscription, snapshot))
    }
    .await;
    match prepared {
        Ok((subscription, snapshot)) => {
            let effect = if from_footer {
                console.show_footer_tasks_at(snapshot, selected_task)?
            } else if let Some(task_id) = selected_task {
                console.show_tasks_at(snapshot, Some(task_id))?
            } else {
                console.show_tasks(snapshot)?
            };
            *task_subscription = Some(subscription);
            apply_task_surface_effect(task_center, console, chat_id, effect).await?;
        }
        Err(error) => outln!(
            console,
            "Cannot open Shell tasks: {}",
            safe_text(&error.to_string())
        ),
    }
    Ok(())
}

async fn open_subscriptions(
    agent: &Agent,
    console: &Console,
    chat_id: uuid::Uuid,
) -> Result<(), String> {
    match agent.tick_subscriptions(chat_id).await {
        Ok(rows) => console.show_subscriptions(chat_id, rows),
        Err(error) => {
            outln!(
                console,
                "Cannot open Tick Subscriptions: {}",
                safe_text(&error.to_string())
            );
            Ok(())
        }
    }
}

async fn refresh_subscriptions(
    agent: &Agent,
    console: &Console,
    chat_id: uuid::Uuid,
) -> Result<(), String> {
    match agent.tick_subscriptions(chat_id).await {
        Ok(rows) => console.update_subscriptions(chat_id, rows),
        Err(error) => console.subscription_change_failed(format!(
            "Cannot read Tick Subscriptions: {}",
            safe_text(&error.to_string())
        )),
    }
}

async fn apply_subscription_effect(
    agent: &Agent,
    console: &Console,
    chat_id: uuid::Uuid,
    effect: SubscriptionEffect,
) -> Result<(), String> {
    if let SubscriptionEffect::Change {
        subscription,
        change,
    } = effect
        && let Err(error) = agent
            .update_tick_subscription(chat_id, subscription, change)
            .await
    {
        return console.subscription_change_failed(format!(
            "Cannot update the Tick Subscription: {}",
            safe_text(&error.to_string())
        ));
    }
    refresh_subscriptions(agent, console, chat_id).await
}

async fn open_diff(agent: &Agent, console: &Console, chat_id: uuid::Uuid) -> Result<(), String> {
    let snapshot = match agent.snapshot(chat_id).await {
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
    console.show_diff(chat_id, catalog)
}

async fn apply_task_surface_effect(
    task_center: &ShellTaskCenter,
    console: &Console,
    chat_id: uuid::Uuid,
    mut effect: TaskSurfaceEffect,
) -> Result<(), String> {
    loop {
        match effect {
            TaskSurfaceEffect::None | TaskSurfaceEffect::Close => return Ok(()),
            TaskSurfaceEffect::Control { task_id, action } => {
                match task_center.dispatch(chat_id, task_id, action).await {
                    Ok(_) => console.clear_task_action_error()?,
                    Err(error) => console.task_action_failed(
                        task_id,
                        format!(
                            "Cannot update Shell task: {}",
                            safe_text(&error.to_string())
                        ),
                    )?,
                }
                match task_center.snapshot(chat_id).await {
                    Ok(snapshot) => effect = console.update_tasks(snapshot)?,
                    Err(error) => {
                        console.task_updates_failed(chat_id, safe_text(&error.to_string()))?;
                        return Ok(());
                    }
                }
            }
            TaskSurfaceEffect::ReadOutput {
                task_id,
                cursor,
                limit,
                generation,
            } => {
                match task_center
                    .read_output(chat_id, task_id, cursor, limit)
                    .await
                {
                    Ok(chunk) => {
                        effect = console
                            .update_task_output(TaskOutputUpdate::Chunk { generation, chunk })?;
                        continue;
                    }
                    Err(error) => {
                        let total_bytes = task_center
                            .snapshot(chat_id)
                            .await
                            .ok()
                            .and_then(|snapshot| {
                                snapshot.tasks.into_iter().find(|task| task.id == task_id)
                            })
                            .map_or(0, |task| task.output_bytes);
                        let _ = console.update_task_output(TaskOutputUpdate::Unavailable {
                            generation,
                            task_id,
                            total_bytes,
                            message: safe_text(&error.to_string()),
                        })?;
                    }
                }
                return Ok(());
            }
        }
    }
}

async fn receive_shell_task_event(
    subscription: &mut Option<ShellTaskSubscription>,
) -> Option<Result<ShellTaskSubscriptionEvent, tickloop_agent::runtime::ShellTaskCenterError>> {
    match subscription {
        Some(subscription) => Some(subscription.recv().await),
        None => std::future::pending().await,
    }
}

async fn check_shell_prompt_watchdog(
    task_center: &ShellTaskCenter,
    console: &Console,
    chat_id: uuid::Uuid,
    watchdog: &mut ShellTaskPromptWatchdog,
    now: std::time::Duration,
) -> Result<(), String> {
    const PROMPT_TAIL_BYTES: u64 = 4096;

    let snapshot = task_center
        .snapshot(chat_id)
        .await
        .map_err(|error| error.to_string())?;
    for task in &snapshot.tasks {
        let trailing_output = if task.state == ShellTaskState::Background && task.output_bytes > 0 {
            let cursor = task.output_bytes.saturating_sub(PROMPT_TAIL_BYTES);
            let limit = usize::try_from(task.output_bytes - cursor)
                .unwrap_or(PROMPT_TAIL_BYTES as usize)
                .max(1);
            task_center
                .read_output(chat_id, task.id, cursor, limit)
                .await
                .map(|chunk| chunk.output)
                .unwrap_or_default()
        } else {
            String::new()
        };
        if let Some(ShellTaskNotice::InteractivePrompt { task_id, .. }) =
            watchdog.observe(task, &trailing_output, now)
        {
            let task_id = task_id.simple().to_string();
            outln!(
                console,
                "\nBackground Shell {} may be waiting for input. Open /tasks to inspect it.",
                &task_id[..8]
            );
        }
    }
    Ok(())
}

/// 照 Claude Code 的任务通知样式：状态色圆点加摘要。
fn task_notification_line(
    notification: &TaskNotification,
    display: DisplayProfile,
) -> Vec<(String, Option<Role>)> {
    let (glyph, role) = match notification.status {
        TaskNotificationStatus::Completed => (Glyph::Success, Role::Success),
        TaskNotificationStatus::Failed => (Glyph::Failure, Role::Error),
        TaskNotificationStatus::Killed => (Glyph::Warning, Role::Warning),
    };
    vec![
        ("\n".into(), None),
        (display.glyph(glyph).into(), Some(role)),
        (format!(" {}\n", safe_text(&notification.summary)), None),
    ]
}

async fn render_user_shell_update(
    agent: &Agent,
    console: &Console,
    record: &UserShellRecord,
    offsets: &mut HashMap<uuid::Uuid, u64>,
) -> Result<bool, String> {
    let is_terminal = matches!(
        record.state,
        UserShellState::Completed | UserShellState::Failed | UserShellState::Unknown
    );
    let mut offset = *offsets.get(&record.id).unwrap_or(&0);
    while record.output_bytes > offset {
        let result = match agent
            .read_user_shell_output(record.chat_id, record.id, offset, 64 * 1024)
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
        if let Some(output) = value["output"].as_str() {
            out!(console, "{}", safe_text(output));
        }
        let next_offset = value["next_offset"]
            .as_u64()
            .or_else(|| value["total_bytes"].as_u64())
            .unwrap_or(offset);
        if next_offset <= offset {
            break;
        }
        offset = next_offset;
        offsets.insert(record.id, offset);
        if !is_terminal {
            break;
        }
    }
    if !is_terminal || offset < record.output_bytes {
        return Ok(false);
    }
    if !console.is_interactive() {
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
    offsets.remove(&record.id);
    Ok(true)
}

#[derive(Debug, PartialEq, Eq)]
struct UserShellObservation {
    render_in_chat: bool,
    /// `Some` 表示该 Chat 的前台 Shell 状态需要改为此值。
    foreground: Option<bool>,
}

/// 按 `!` 命令的状态记录调整前台 Shell。用户状态事件可能晚于转入后台的处理才被读到，
/// 已转入后台的命令记在 `backgrounded_shells` 中，旧的"运行中"记录不能把它放回前台；
/// 转入后台的记录一律离开前台，结束后不再跟踪。
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
    let was_foreground = foreground_shells.get(&record.chat_id) == Some(&record.id);
    let mut foreground = None;
    if record.state == UserShellState::Running && !is_backgrounded {
        foreground_shells.insert(record.chat_id, record.id);
        foreground = Some(true);
    }
    let is_foreground = foreground_shells.get(&record.chat_id) == Some(&record.id);
    let render_in_chat = !is_backgrounded && (was_foreground || is_foreground || is_terminal);
    if (is_terminal || is_backgrounded) && is_foreground {
        foreground_shells.remove(&record.chat_id);
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

fn user_shell_is_terminal(record: &UserShellRecord) -> bool {
    matches!(
        record.state,
        UserShellState::Completed | UserShellState::Failed | UserShellState::Unknown
    )
}

fn sync_snapshot(
    snapshot: &tickloop_agent::runtime::ChatSnapshot,
    pending: &mut HashMap<uuid::Uuid, Receipt>,
    permissions: &mut HashMap<uuid::Uuid, uuid::Uuid>,
    questions: &mut HashMap<uuid::Uuid, QuestionRequest>,
) {
    use tickloop_agent::runtime::LoopState;
    sync_snapshot_requests(snapshot, pending, permissions, questions);
    pending.retain(|_, receipt| receipt.chat_id != snapshot.id);
    for input in snapshot.pending_inputs.iter().filter(|input| input.armed) {
        pending.insert(input.receipt.run_id, input.receipt.clone());
    }
    for goal in &snapshot.loops {
        if matches!(
            goal.state,
            LoopState::Running | LoopState::WaitingPermission | LoopState::WaitingQuestion
        ) && let Some(run_id) = goal.run_id
        {
            pending.insert(
                run_id,
                Receipt {
                    chat_id: snapshot.id,
                    loop_id: goal.id,
                    run_id,
                },
            );
        }
    }
}

fn sync_snapshot_requests(
    snapshot: &tickloop_agent::runtime::ChatSnapshot,
    pending: &HashMap<uuid::Uuid, Receipt>,
    permissions: &mut HashMap<uuid::Uuid, uuid::Uuid>,
    questions: &mut HashMap<uuid::Uuid, QuestionRequest>,
) {
    permissions.retain(|_, run| {
        pending
            .get(run)
            .is_some_and(|receipt| receipt.chat_id != snapshot.id)
    });
    for request in &snapshot.permissions {
        permissions.insert(request.id, request.run_id);
    }
    questions.retain(|_, question| question.chat_id != snapshot.id);
    for request in &snapshot.questions {
        questions.insert(request.id, request.clone());
    }
}

async fn restore_view(
    console: &Console,
    agent: &Agent,
    tools: &mut ToolPresentation,
    chat: uuid::Uuid,
    after: u64,
    buffered_replies: &mut HashMap<uuid::Uuid, ReplyLines>,
) -> Result<tickloop_agent::runtime::Observation, String> {
    let observation = agent
        .observe(chat, after)
        .await
        .map_err(|error| error.to_string())?;
    let conversation_events = observation.snapshot.conversation_events();
    console.select_chat(Some(chat))?;
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
                buffered_replies.remove(&event.run_id);
            } else if event.sequence <= after
                && matches!(event.kind, EventKind::Completed { .. })
                && let Some(mut reply) = buffered_replies.remove(&event.run_id)
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
    if !console.is_interactive() {
        outln!(
            console,
            "Chat {chat} restored. Current model: {} · {:?}",
            safe_text(&observation.snapshot.model.model),
            observation.snapshot.model.protocol
        );
    }
    if observation.reset_required {
        console.reset_chat(chat)?;
        tools.restore(&conversation_events);
        outln!(
            console,
            "The saved event range changed; displaying the retained conversation."
        );
        workflows::write_public_history(console, &observation.snapshot)?;
        for goal in observation
            .snapshot
            .loops
            .iter()
            .filter(|goal| goal.state == tickloop_agent::runtime::LoopState::Failed)
        {
            let error=observation.snapshot.events.iter().rev().find_map(|event| {
                if event.loop_id==goal.id && let EventKind::Failed(error)=&event.kind {Some(error.as_str())} else {None}
            }).unwrap_or("The execution failed before this View received its final update; inspect the saved execution history.");
            outln!(console, "Failed: {}", safe_text(error));
        }
    } else {
        let mut thinking_runs = HashSet::new();
        let mut message_groups = MessageGroups::default();
        for event in conversation_events
            .iter()
            .filter(|event| event.sequence <= after)
        {
            message_groups.message_id(event);
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
                show_consumed_agent_message(console, &observation.snapshot, event.run_id)?;
            }
            if matches!(
                event.kind,
                EventKind::Started | EventKind::InputMerged { .. }
            ) && let Some(input) = consumed_input_text(&observation.snapshot, event.run_id)
            {
                console.record_consumed_input(chat, event.run_id, &safe_text(input))?;
            }
            if matches!(
                event.kind,
                EventKind::Thinking(_)
                    | EventKind::ToolStarted { .. }
                    | EventKind::PermissionRequested { .. }
                    | EventKind::QuestionAsked { .. }
                    | EventKind::ModelRetry { .. }
                    | EventKind::Completed { .. }
                    | EventKind::Interrupted
            ) && let Some(mut reply) = buffered_replies.remove(&event.run_id)
            {
                reply.finish();
                write_pending_reply_lines(console, &mut reply)?;
            }
            if matches!(event.kind, EventKind::Failed(_)) {
                buffered_replies.remove(&event.run_id);
            }
            let message_id = message_groups.message_id(event);
            let console = &console.for_message(message_id.clone());
            if matches!(event.kind, EventKind::Thinking(_)) {
                thinking_runs.insert(event.run_id);
            } else if ends_thinking(&event.kind)
                && thinking_runs.remove(&event.run_id)
                && console.is_interactive()
            {
                console.write_roles(&[(
                    "\n∴ Thinking (ctrl+o to expand)\n".into(),
                    Some(Role::MutedText),
                )])?;
            }
            match &event.kind {
                EventKind::ToolCompleted { call_id, .. } => {
                    console.finish_tool_call(Some(call_id))?
                }
                EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted => {
                    console.finish_tool_call(None)?
                }
                _ => {}
            }
            tools.record(event);
            if let Some(rendered) = tools.render_event_roles(event, console.display_profile()?) {
                match &event.kind {
                    EventKind::ToolStarted { call_id, .. }
                        if console.is_interactive() && !tools.is_detailed() =>
                    {
                        console.write_tool_call(call_id, &rendered)?
                    }
                    _ => console.write_roles(&rendered)?,
                }
                continue;
            }
            match &event.kind {
                EventKind::Thinking(thinking) => {
                    if !console.is_interactive() {
                        console.write(&safe_text(thinking))?;
                    }
                }
                EventKind::Text(text) if !failed_runs.contains(&event.run_id) => {
                    if console.is_interactive() {
                        let reply = buffered_replies
                            .entry(event.run_id)
                            .or_insert_with(|| ReplyLines::new(event.chat_id, message_id));
                        reply.push(&safe_text(text));
                        write_pending_reply_lines(console, reply)?;
                    } else {
                        console.write_markdown(&safe_text(text), false)?;
                    }
                }
                EventKind::Completed { usage } => {
                    write_completion_notice(console, usage)?;
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
                } => outln!(
                    console,
                    "{}",
                    model_retry_notice(*kind, error, *attempt, *max_retries, *delay_ms),
                ),
                EventKind::Interrupted => outln!(console, "\nInterrupted"),
                EventKind::GoalEnded { reason, operations } => {
                    print_goal_end(console, event.loop_id, reason, operations)?
                }
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
                EventKind::PlanUpdated { plan } => outln!(
                    console,
                    "\nPlan version {} for goal {}:\n{}",
                    plan.version,
                    plan.loop_id,
                    safe_text(&plan.markdown)
                ),
                EventKind::ProgressUpdated { progress } => {
                    for todo in &progress.todos {
                        outln!(
                            console,
                            "\nProgress {:?}: {}",
                            todo.status,
                            safe_text(&todo.content)
                        );
                    }
                }
                EventKind::PermissionChecked { decision, reason } => outln!(
                    console,
                    "\nAuto review: {decision:?} · {}",
                    safe_text(reason)
                ),
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
        for goal in &observation.snapshot.loops {
            outln!(
                console,
                "Goal: {} · {:?} · {}",
                goal.id,
                goal.state,
                safe_text(&goal.goal)
            );
        }
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
                "Saved input awaiting manual continuation: Goal {} · Run {}\n{}",
                input.receipt.loop_id,
                input.receipt.run_id,
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
            "Permission request: {} · {}\nOperation: {}\nUse /allow {} [once|session|persistent] or /deny {}.",
            request.id,
            safe_text(&request.name),
            safe_text(&request.input.to_string()),
            request.id,
            request.id
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

    #[cfg(test)]
    agent.record_recovery_fact(recovery_tests::RecoveryFact::Restored {
        chat,
        after,
        last_sequence: observation.snapshot.last_sequence,
        reset_required: observation.reset_required,
    });
    Ok(observation)
}

fn print_question(console: &Console, request: &QuestionRequest) -> Result<(), String> {
    if console.is_interactive() {
        let questions = request
            .questions
            .iter()
            .map(|question| safe_line(&question.question))
            .collect::<Vec<_>>();
        outln!(console, "\nYour input is needed: {}", questions.join(" · "));
        return Ok(());
    }
    outln!(
        console,
        "\nQuestion: {} · goal {}",
        request.id,
        request.loop_id
    );
    let total = request.questions.len();
    for (index, question) in request.questions.iter().enumerate() {
        for line in questions::question_lines(question, index, total) {
            outln!(console, "{line}");
        }
    }
    outln!(
        console,
        "Answer each question on its own line: an option number or label, numbers separated by commas where more than one is allowed, or your own text. /answer {} ANSWER answers its next question.",
        request.id
    );
    Ok(())
}

/// Answers the next question of `request` with typed text; returns whether every question of
/// the request has now been answered.
async fn answer_typed_question(
    agent: &Agent,
    console: &Console,
    menus: &mut Menus,
    questions: &mut HashMap<uuid::Uuid, QuestionRequest>,
    request: &QuestionRequest,
    text: &str,
) -> Result<bool, String> {
    match menus.answer_typed(agent, console, request, text).await {
        Ok(None) => {
            questions.remove(&request.id);
            outln!(console, "Answer accepted.");
            Ok(true)
        }
        Ok(Some(next_question)) => {
            outln!(console, "Answer recorded. Next question:");
            for line in next_question {
                outln!(console, "{line}");
            }
            Ok(false)
        }
        Err(error) => {
            outln!(console, "Cannot answer question: {}", safe_text(&error));
            Ok(false)
        }
    }
}

fn print_goal_end(
    console: &Console,
    goal: uuid::Uuid,
    reason: &str,
    operations: &[tickloop_agent::runtime::OperationRecord],
) -> Result<(), String> {
    outln!(console, "\nGoal {goal} ended: {}", safe_text(reason));
    for operation in operations {
        outln!(
            console,
            "Operation {} remains {:?}: {}",
            operation.id,
            operation.state,
            safe_text(&operation.name)
        );
    }
    Ok(())
}

async fn trigger_control(
    console: &Console,
    agent: &Agent,
    chat: uuid::Uuid,
    goal: uuid::Uuid,
    command: &str,
) -> Result<Option<Receipt>, String> {
    if let Some(arguments) = command.strip_prefix("/trigger ") {
        let mut fields = arguments.splitn(2, ' ');
        let source = fields.next().ok_or("event source is required")?.to_owned();
        let payload = fields
            .next()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|_| "event payload must be valid JSON")?
            .unwrap_or(serde_json::Value::Null);
        return agent
            .trigger(chat, goal, TriggerSource { source, payload })
            .await
            .map(Some)
            .map_err(|error| error.to_string());
    }
    let snapshot = agent
        .snapshot(chat)
        .await
        .map_err(|error| error.to_string())?;
    let current = snapshot
        .loops
        .iter()
        .find(|item| item.id == goal)
        .ok_or("goal does not exist")?
        .trigger
        .clone();
    if command == "/clear-trigger" {
        agent
            .configure_trigger(chat, goal, None)
            .await
            .map_err(|error| error.to_string())?;
        outln!(console, "Trigger disabled for goal {goal}.");
        return Ok(None);
    }
    let mut trigger = current.unwrap_or(Trigger {
        id: uuid::Uuid::new_v4(),
        prompt: String::new(),
        interval_seconds: None,
        expires_at_ms: None,
        accepts_events: false,
        armed: true,
        subscriptions: Vec::new(),
    });
    trigger.id = uuid::Uuid::new_v4();
    trigger.armed = true;
    if let Some(prompt) = command.strip_prefix("/events ") {
        trigger.prompt = prompt.trim().into();
        trigger.accepts_events = true;
    }
    agent
        .configure_trigger(chat, goal, Some(trigger.clone()))
        .await
        .map_err(|error| error.to_string())?;
    print_trigger(console, &trigger)?;
    Ok(None)
}

async fn parse_after_turn(
    agent: &Agent,
    chat: uuid::Uuid,
    selected_loop: Option<uuid::Uuid>,
    command: &str,
) -> Result<crate::agent_access::AfterTurnAction, AgentError> {
    use crate::agent_access::{
        AfterTurnAction, ConfigurationChange, HistoryAction, LoopAction, UserInput,
    };
    let invalid = |message: &str| AgentError::Invalid(message.into());
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
        "/search-config" => {
            let config = if arguments == "off" {
                None
            } else {
                let path = agent.snapshot(chat).await?.workdir.join(arguments);
                let bytes = std::fs::read(path)
                    .map_err(|_| invalid("cannot read search configuration file"))?;
                Some(
                    serde_json::from_slice(&bytes)
                        .map_err(|_| invalid("invalid search configuration JSON"))?,
                )
            };
            Ok(AfterTurnAction::Configure {
                change: ConfigurationChange::Search { config },
            })
        }
        "/cwd" | "/add-dir" => {
            let mut snapshot = agent.snapshot(chat).await?;
            if name == "/cwd" {
                snapshot.workdir = snapshot.workdir.join(arguments);
            } else {
                snapshot
                    .additional_directories
                    .push(snapshot.workdir.join(arguments));
            }
            Ok(AfterTurnAction::Configure {
                change: ConfigurationChange::Directories {
                    workdir: snapshot.workdir,
                    additional: snapshot.additional_directories,
                },
            })
        }
        "/rule" => {
            let fields = arguments.split_whitespace().collect::<Vec<_>>();
            let view = agent.permissions_view(chat).await?;
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
                        id: uuid::Uuid::new_v4(),
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
                    "use /rule allow|ask|deny TOOL [session|persistent], or /rule remove UUID",
                ));
            };
            Ok(AfterTurnAction::Configure {
                change: ConfigurationChange::PermissionChange {
                    change,
                    expected: view.revision,
                },
            })
        }
        "/loops" => {
            let loops::LoopCommand::Schedule { goal, arguments } =
                loops::LoopCommand::parse(command).map_err(invalid)?
            else {
                return Err(invalid(
                    "only timer scheduling is an after-turn Loop command",
                ));
            };
            let schedule = loops::TimerSchedule::parse(arguments).map_err(invalid)?;
            let snapshot = agent.snapshot(chat).await?;
            let current = snapshot
                .loops
                .iter()
                .find(|item| item.id == goal)
                .ok_or(AgentError::NotFound)?
                .trigger
                .clone();
            let trigger = schedule.prepare(current).map_err(AgentError::Invalid)?;
            Ok(AfterTurnAction::Loop {
                action: LoopAction::ConfigureTrigger {
                    loop_id: goal,
                    trigger: Some(trigger),
                },
            })
        }
        "/clear-trigger" => Ok(AfterTurnAction::Loop {
            action: LoopAction::ConfigureTrigger {
                loop_id: selected_loop.ok_or_else(|| invalid("select a Loop first"))?,
                trigger: None,
            },
        }),
        "/trigger" => {
            let (source, payload) = arguments
                .split_once(char::is_whitespace)
                .unwrap_or((arguments, "null"));
            let payload = serde_json::from_str(payload)
                .map_err(|_| invalid("event payload must be valid JSON"))?;
            Ok(AfterTurnAction::Loop {
                action: LoopAction::EmitTrigger {
                    loop_id: selected_loop.ok_or_else(|| invalid("select a Loop first"))?,
                    source: TriggerSource {
                        source: source.into(),
                        payload,
                    },
                },
            })
        }
        "/events" => {
            let loop_id = selected_loop.ok_or_else(|| invalid("select a Loop first"))?;
            let snapshot = agent.snapshot(chat).await?;
            let mut trigger = snapshot
                .loops
                .iter()
                .find(|item| item.id == loop_id)
                .ok_or(AgentError::NotFound)?
                .trigger
                .clone()
                .unwrap_or(Trigger {
                    id: uuid::Uuid::new_v4(),
                    prompt: String::new(),
                    interval_seconds: None,
                    expires_at_ms: None,
                    accepts_events: false,
                    armed: true,
                    subscriptions: Vec::new(),
                });
            trigger.id = uuid::Uuid::new_v4();
            trigger.armed = true;
            trigger.accepts_events = true;
            trigger.prompt = arguments.into();
            Ok(AfterTurnAction::Loop {
                action: LoopAction::ConfigureTrigger {
                    loop_id,
                    trigger: Some(trigger),
                },
            })
        }
        "/approve-plan" => {
            let fields = arguments.split_whitespace().collect::<Vec<_>>();
            if fields.len() != 3 {
                return Err(invalid("use /approve-plan GOAL VERSION MODE"));
            }
            let loop_id = fields[0]
                .parse()
                .map_err(|_| invalid("goal ID must be a UUID"))?;
            let version = fields[1]
                .parse()
                .map_err(|_| invalid("plan version must be a number"))?;
            let mode = serde_json::from_value(serde_json::Value::String(fields[2].into()))
                .map_err(|_| invalid("invalid execution mode"))?;
            Ok(AfterTurnAction::Loop {
                action: LoopAction::ApprovePlan {
                    loop_id,
                    version,
                    mode,
                },
            })
        }
        _ => {
            let skill = name
                .strip_prefix('/')
                .ok_or_else(|| invalid("after-turn actions must be explicit commands"))?;
            if !agent.has_skill(chat, skill).await? {
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

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn print_trigger(console: &Console, trigger: &Trigger) -> Result<(), String> {
    outln!(
        console,
        "Trigger: {} · armed {} · explicit events {}",
        trigger.id,
        trigger.armed,
        trigger.accepts_events
    );
    if let Some(seconds) = trigger.interval_seconds {
        outln!(
            console,
            "Interval: {seconds} seconds while this Agent runs."
        );
    }
    if let Some(expiry) = trigger.expires_at_ms {
        outln!(
            console,
            "Expires in {} seconds.",
            expiry.saturating_sub(current_time_ms()) / 1000
        );
    }
    outln!(console, "Prompt: {}", safe_text(&trigger.prompt));
    Ok(())
}

async fn command_control(
    console: &Console,
    agent: &Agent,
    chat: uuid::Uuid,
    command: &str,
) -> Result<(), String> {
    let fields = command.split_whitespace().collect::<Vec<_>>();
    if is_task_surface_command(command) {
        for command in agent
            .commands(chat)
            .await
            .map_err(|error| error.to_string())?
        {
            print_command(console, &command)?;
        }
        return Ok(());
    }
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
            .read_command(chat, operation, offset, limit)
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
            agent.background_command(chat, operation).await
        } else {
            agent.stop_command(chat, operation).await
        }
        .map_err(|error| error.to_string())?;
        print_command(console, &state)?;
    }
    Ok(())
}

fn print_command(
    console: &Console,
    command: &tickloop_agent::tools::CommandSnapshot,
) -> Result<(), String> {
    outln!(
        console,
        "\nCommand: {} · {:?} · PID {:?} · goal {}\n{}\nDirectory: {}\nExit code: {:?} · Signal: {:?} · Output bytes: {}\nFull output: {}",
        command.operation_id,
        command.state,
        command.pid,
        command.loop_id,
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

#[cfg(test)]
fn read_config(path: &std::path::Path) -> Result<ModelConfig, String> {
    read_config_with_provider(path, None).map(|(config, _)| config)
}

fn read_config_with_provider(
    path: &std::path::Path,
    user_settings: Option<&UserSettings>,
) -> Result<(ModelConfig, Option<ProviderPatch>), String> {
    let bytes = std::fs::read(path).map_err(|_| "cannot read the model configuration; check the configuration path and file permissions")?;
    if path
        .extension()
        .is_some_and(|extension| extension == "yaml" || extension == "yml")
    {
        let contents = std::str::from_utf8(&bytes)
            .map_err(|error| format!("invalid provider YAML encoding: {error}"))?;
        let catalog = user_settings
            .and_then(|settings| settings.persisted_model_catalog().ok())
            .unwrap_or_else(tickloop_agent::model::cards::ModelCatalog::defaults);
        let (config, provider) =
            tickloop_agent::model::cards::ModelCatalog::resolved_provider_yaml_with_catalog(
                contents, catalog,
            )?;
        return Ok((config, Some(provider)));
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid model configuration JSON: {error}; see crates/agent/examples/providers/deepseek-api.yaml for the current configuration format"))?;
    if value.get("providers").is_some() {
        let catalog: tickloop_agent::model::cards::ModelCatalogFile = serde_json::from_value(value)
            .map_err(|error| format!("invalid providers configuration: {error}"))?;
        catalog.selected_model().map(|config| (config, None))
    } else {
        let legacy: ModelConfig = serde_json::from_value(value)
            .map_err(|error| format!("invalid model configuration: {error}"))?;
        Ok((
            tickloop_agent::model::cards::model_for_persistent_selection(legacy),
            None,
        ))
    }
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

/// Tick Loop 状态根目录：`TICKLOOP_CONFIG_DIR` 非空时使用它，否则为用户主目录下的 `.tickloop`。
/// 主目录取自 `std::env::home_dir()`：`$HOME` 缺失或为空时回退到账户数据库。
/// 两者都不可用时返回 `None`，调用方不写盘。
pub(crate) fn config_root_directory() -> Option<PathBuf> {
    std::env::var_os("TICKLOOP_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::home_dir()
                .filter(|home| !home.as_os_str().is_empty())
                .map(|home| home.join(".tickloop"))
        })
}

async fn ensure_chat(
    console: &Console,
    agent: &Agent,
    selected: &mut Option<uuid::Uuid>,
    workdir: &std::path::Path,
) -> Result<uuid::Uuid, String> {
    if let Some(id) = selected
        && !agent.first_create_pending(*id)
    {
        return Ok(*id);
    }
    let draft = agent
        .configuration(ConfigTarget::NewDraft)
        .await
        .map_err(|error| error.to_string())?;
    let startup_banner = startup_banner_spans(&draft.model, workdir);
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
                "New Agent created; staged choice {command_id} has an unknown result and was not resent. Send again once its receipt is available, or /acknowledge-create {command_id} to cancel the remaining choices. Input kept and not sent"
            )),
        ),
        Err(DraftCreationError::ChoiceRefused {
            chat,
            command_id,
            error,
        }) => (
            chat,
            Some(format!(
                "New Agent created, but staged choice {command_id} was refused: {}. /acknowledge-create {command_id} cancels the remaining choices. Input kept and not sent",
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
            return Err(format!(
                "New Agent creation {command_id} is unconfirmed; nothing was resent. Send again after reconnecting to check its receipt; input kept"
            ));
        }
    };
    *selected = Some(id);
    console.select_created_chat(id, &startup_banner)?;
    console.set_input_context(Some(id), workdir, &[])?;
    agent
        .display_chat(Some(id))
        .await
        .map_err(|error| error.to_string())?;
    if !console.is_interactive() {
        outln!(console, "Chat: {id}");
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
    into_run: uuid::Uuid,
    sequence: u64,
    echo: InputEcho,
    submission_block: Option<u64>,
}

impl MergedEchoes {
    fn accepted(&mut self, run: uuid::Uuid, echo: Option<InputEcho>) {
        self.input_blocks
            .entry(run)
            .or_insert(self.submission_block);
        self.consumed_echo(run, echo);
    }

    fn consumed_echo(&mut self, run: uuid::Uuid, echo: Option<InputEcho>) {
        if let Some(echo) = echo {
            self.accepted.insert(run, echo);
        }
    }

    /// Follows one event of a chat shown through `displayed_through`; returns the two echoes to
    /// show as one block, if any. An event a restore or reload already showed only ends the block.
    fn observe(
        &mut self,
        event: &tickloop_agent::runtime::Event,
        displayed_through: u64,
    ) -> Option<(InputEcho, InputEcho)> {
        let previous = self.open.remove(&event.chat_id);
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
            .then(|| self.accepted.remove(&event.run_id))
            .flatten();
        let submission_block = consumed
            .then(|| self.input_blocks.remove(&event.run_id))
            .flatten();
        if event.sequence <= displayed_through {
            return None;
        }
        let EventKind::InputMerged { into_run_id } = &event.kind else {
            return None;
        };
        let current = current?;
        self.open.insert(
            event.chat_id,
            MergeCursor {
                into_run: *into_run_id,
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
    selected: &mut Option<uuid::Uuid>,
    workdir: &std::path::Path,
    retry_of: Option<uuid::Uuid>,
    input: &str,
    echo: &mut Option<InputEcho>,
) -> Result<Result<uuid::Uuid, String>, String> {
    let echoed_in = *selected;
    let creates_agent =
        retry_of.is_none() && echoed_in.is_none_or(|chat| agent.first_create_pending(chat));
    let chat = match ensure_chat(console, agent, selected, workdir).await {
        Ok(chat) => chat,
        Err(error) => {
            if creates_agent && let Some(echo) = echo.take() {
                console.withdraw_echo(echoed_in, echo)?;
                if *selected != echoed_in {
                    console.withdraw_echo(*selected, echo)?;
                }
            }
            return Ok(Err(error));
        }
    };
    if creates_agent && echoed_in != Some(chat) {
        if let Some(echo) = echo.take() {
            console.withdraw_echo(echoed_in, echo)?;
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
    Unknown(uuid::Uuid, AgentError),
    /// An earlier input's acceptance is still unknown, so this one was not sent.
    Held(AgentError),
    Failed(AgentError),
}

/// Sends an input to `chat`. For an unchanged restored draft (`retry_of`), it first reads the
/// earlier attempt's receipt: an accepted attempt is adopted and nothing is sent, an unknown one
/// stays held, and a refused one lets this input go out as new.
async fn send_input<T>(
    agent: &Agent,
    chat: uuid::Uuid,
    retry_of: Option<uuid::Uuid>,
    send: impl std::future::Future<Output = Result<T, AgentError>>,
    adopt: impl FnOnce(crate::agent_access::CommandResult) -> Option<T>,
) -> InputSend<T> {
    if let Some(id) = retry_of {
        match agent.resolve_input(chat, id).await {
            InputResolution::Accepted(result) => {
                return match adopt(result) {
                    Some(value) => InputSend::Accepted {
                        value,
                        adopted: Some(id),
                    },
                    None => InputSend::Failed(AgentError::Invalid(format!(
                        "Input {id} was accepted as another kind of input; nothing was resent"
                    ))),
                };
            }
            InputResolution::Unknown => {
                return InputSend::Unknown(
                    id,
                    AgentError::Invalid(format!(
                        "Input {id} still has an unknown acceptance result; nothing was resent. /acknowledge-input {id} releases it without resending"
                    )),
                );
            }
            InputResolution::Refused | InputResolution::Released => {}
        }
    }
    let before = agent.unknown_input(chat);
    match send.await {
        Ok(value) => InputSend::Accepted {
            value,
            adopted: None,
        },
        Err(error) => match agent.unknown_input(chat) {
            Some(id) if Some(id) != before => InputSend::Unknown(id, error),
            Some(_) => InputSend::Held(error),
            None => InputSend::Failed(error),
        },
    }
}

fn submitted_receipt(result: crate::agent_access::CommandResult) -> Option<Receipt> {
    match result {
        crate::agent_access::CommandResult::Submitted(receipt) => Some(receipt),
        _ => None,
    }
}

fn input_adopted(id: uuid::Uuid) -> String {
    format!("Input {id} was already accepted; nothing was resent")
}

fn consumed_input_text(snapshot: &ChatSnapshot, input: uuid::Uuid) -> Option<&str> {
    snapshot
        .messages
        .iter()
        .enumerate()
        .find_map(|(index, message)| {
            if snapshot.message_runs.get(&index) != Some(&input)
                || message.role != tickloop_agent::model::Role::User
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
                tickloop_agent::model::Content::Text { text }
                    if presentation::typed_local_message(text, false).is_none()
                        && !conversation::is_interruption_placeholder(message.role, text) =>
                {
                    Some(text.as_str())
                }
                _ => None,
            })
        })
}

fn show_consumed_agent_message(
    console: &Console,
    snapshot: &ChatSnapshot,
    run: uuid::Uuid,
) -> Result<(), String> {
    for (index, message) in snapshot.messages.iter().enumerate() {
        let Some(id) = snapshot
            .message_ids
            .get(&index)
            .filter(|id| snapshot.agent_message_origins.contains_key(id))
        else {
            continue;
        };
        if snapshot.message_runs.get(&index) != Some(&run)
            || console.input_is_echoed(snapshot.id, *id)?
        {
            continue;
        }
        for content in &message.content {
            if let tickloop_agent::model::Content::Text { text } = content
                && let Some(
                    local @ tickloop_agent::runtime::LocalMessage::AgentCommunication { .. },
                ) = tickloop_agent::runtime::LocalMessage::parse(text)
            {
                console.write_roles(&[(
                    presentation::local_message_text(
                        &local,
                        presentation::OutputDetail::Preview,
                        console.display_profile()?,
                    ),
                    Some(Role::MutedText),
                )])?;
            }
        }
        console.mark_input_echoes(snapshot.id, [*id])?;
    }
    Ok(())
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
    if let Ok(snapshot) = agent.snapshot(receipt.chat_id).await
        && let Some(input) = consumed_input_text(&snapshot, receipt.run_id)
    {
        if echo.is_some() {
            console.mark_input_echoes(receipt.chat_id, [receipt.run_id])?;
            return Ok(echo);
        }
        return console.record_consumed_input(receipt.chat_id, receipt.run_id, &safe_text(input));
    }
    if let Some(echo) = echo {
        console.withdraw_echo(Some(receipt.chat_id), echo)?;
    }
    Ok(None)
}

async fn show_consumed_input(
    console: &Console,
    agent: &Agent,
    event: &tickloop_agent::runtime::Event,
) -> Result<Option<InputEcho>, String> {
    if !console.is_interactive() || console.input_is_echoed(event.chat_id, event.run_id)? {
        return Ok(None);
    }
    console.defer_input_echo(event.chat_id, event.run_id)?;
    let Ok(snapshot) = agent.snapshot(event.chat_id).await else {
        return Ok(None);
    };
    consume_input_echo_from_snapshot(console, &snapshot, event.run_id)
}

fn consume_input_echo_from_snapshot(
    console: &Console,
    snapshot: &ChatSnapshot,
    input: uuid::Uuid,
) -> Result<Option<InputEcho>, String> {
    match consumed_input_text(snapshot, input) {
        Some(text) => console.record_consumed_input(snapshot.id, input, &safe_text(text)),
        None => {
            let mut has_unshown_agent_message = false;
            for (index, id) in &snapshot.message_ids {
                if snapshot.message_runs.get(index) == Some(&input)
                    && snapshot.agent_message_origins.contains_key(id)
                    && !console.input_is_echoed(snapshot.id, *id)?
                {
                    has_unshown_agent_message = true;
                    break;
                }
            }
            if !has_unshown_agent_message && snapshot.message_runs.values().any(|run| *run == input)
            {
                console.finish_deferred_input_echo(snapshot.id, input)?;
            }
            Ok(None)
        }
    }
}

fn reconcile_consumed_input_echoes(
    console: &Console,
    snapshot: &ChatSnapshot,
) -> Result<(), String> {
    let mut inputs = console.awaiting_input_echoes(snapshot.id)?;
    inputs.sort_by_key(|input| {
        snapshot
            .message_runs
            .iter()
            .find_map(|(index, run)| (*run == *input).then_some(*index))
            .unwrap_or(usize::MAX)
    });
    for input in inputs {
        show_consumed_agent_message(console, snapshot, input)?;
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
    permissions: &mut HashMap<uuid::Uuid, uuid::Uuid>,
    questions: &mut HashMap<uuid::Uuid, QuestionRequest>,
) -> Result<(), String> {
    let Some(id) = adopted else {
        pending.insert(receipt.run_id, receipt);
        return Ok(());
    };
    outln!(console, "{}", input_adopted(id));
    if let Ok(snapshot) = agent.snapshot(receipt.chat_id).await {
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
        return fields[2]
            .parse::<uuid::Uuid>()
            .map(RuleCommand::Remove)
            .map_err(|_| "rule ID must be a UUID".into());
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
        id: uuid::Uuid::new_v4(),
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
        rule.id,
        rule.decision,
        safe_text(&rule.tool),
        if rule.persistent {
            "persistent"
        } else {
            "session"
        }
    )
}

async fn update_rule(agent: &Agent, chat: uuid::Uuid, command: &str) -> Result<String, String> {
    let command = parse_rule_command(command)?;
    let view = agent
        .permissions_view(chat)
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
                .apply_permission_change(chat, PermissionChange::RemoveRule { id }, view.revision)
                .await
                .map_err(|error| error.to_string())?;
            Ok(format!("Rule removed: {id}"))
        }
        RuleCommand::Add(rule) => {
            let message = rule_added_message(&rule, "all goals and inputs in this Chat");
            agent
                .apply_permission_change(chat, PermissionChange::UpsertRule { rule }, view.revision)
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
            format!("Rule removed: {id}"),
            format!("Rule removed from the New Agent: {id}"),
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
    let mut additional = agent
        .configuration(ConfigTarget::NewDraft)
        .await
        .map_err(|error| error.to_string())?
        .additional_directories;
    let directory = if let Some(path) = line.strip_prefix("/cwd ") {
        let directory = resolve(path)?;
        global
            .interpret(crate::agent_console::Intent::DispatchDirectory(
                directory.clone(),
            ))
            .map_err(|error| error.to_string())?;
        directory
    } else {
        if let Some(path) = line.strip_prefix("/add-dir ") {
            additional.push(resolve(path)?);
            agent
                .change_configuration(
                    ConfigTarget::NewDraft,
                    ConfigurationChange::Directories {
                        workdir: directory.clone(),
                        additional: additional.clone(),
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
        }
        directory
    };
    Ok((directory, additional))
}

/// 按请求 id 在本进程所有已加载 Chat 中定位所属 Chat；找不到时交给前台 Chat 报告。
async fn answer_permission(
    agent: &Agent,
    selected_chat: Option<uuid::Uuid>,
    command: &str,
) -> Result<uuid::Uuid, String> {
    let fields = command.split_whitespace().collect::<Vec<_>>();
    if !(2..=3).contains(&fields.len()) {
        return Err("use /allow REQUEST [once|session|persistent] or /deny REQUEST".into());
    }
    let request = fields[1]
        .parse()
        .map_err(|_| "permission request ID must be a UUID")?;
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
        .map(|snapshot| snapshot.id)
        .or(selected_chat)
        .ok_or("no Chat is selected")?;
    agent
        .answer_permission_with_scope(chat, request, fields[0] == "/allow", scope)
        .await
        .map_err(|error| error.to_string())?;
    Ok(request)
}

async fn open_chat(
    agent: &Agent,
    command: &str,
    current_chat: Option<uuid::Uuid>,
) -> Result<tickloop_agent::runtime::ChatSnapshot, String> {
    let fields = command.split_whitespace().collect::<Vec<_>>();
    let [_, chat] = fields.as_slice() else {
        return Err("use /resume [CHAT]".into());
    };
    let chat = chat
        .parse::<uuid::Uuid>()
        .map_err(|_| "Chat ID must be a UUID; use /resume to choose a saved conversation")?;
    if current_chat == Some(chat) {
        return Err(format!("Chat {chat} is already open."));
    }
    let snapshot = agent.resume_chat(chat).await.map_err(|error| match error {
        AgentError::NotFound => format!("Chat {chat} was not found."),
        error => format!("Failed to resume Chat {chat}: {error}"),
    })?;
    Ok(snapshot)
}

/// 最近显示过的用户设置警告。启动和每次新建、恢复 Chat 都会读取设置，同一组警告只显示一次；
/// 设置恢复有效后清空，之后再次失效时重新显示。
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

fn write_warning(console: &Console, warning: &str) -> Result<(), String> {
    if console.is_interactive() {
        console.write_roles(&[
            (
                console.display_profile()?.glyph(Glyph::Warning).into(),
                Some(Role::Warning),
            ),
            (format!(" {}\n", safe_line(warning)), None),
        ])?;
    } else {
        outln!(console, "Warning: {}", safe_line(warning));
    }
    Ok(())
}

/// 把刚切到前台的 Chat 历史写入视图；只有一个 goal 时自动选中它。
fn show_opened_chat(
    console: &Console,
    agent: &Agent,
    snapshot: &tickloop_agent::runtime::ChatSnapshot,
    has_history: bool,
) -> Result<Option<uuid::Uuid>, String> {
    console.reset_chat(snapshot.id)?;
    let goal = (snapshot.loops.len() == 1).then(|| snapshot.loops[0].id);
    outln!(console, "Chat: {}", snapshot.id);
    for warning in agent.transcript_load_warnings(snapshot.id) {
        write_warning(console, &warning)?;
    }
    show_settings_warnings(console, agent.user_settings_warnings(snapshot.id))?;
    workflows::write_public_history(console, snapshot)?;
    let action = if goal.is_some() {
        "continue the selected goal"
    } else {
        "start a new goal in this Chat"
    };
    if has_history {
        outln!(console, "History opened. Enter a message to {action}.");
    } else {
        outln!(console, "Enter a message to {action}.");
    }
    Ok(goal)
}

/// What a background notice calls an Agent before this View has received its snapshot.
const UNLABELED_BACKGROUND_AGENT: &str = "Background Agent · label unavailable · open Agents";

/// A background Agent as notices name it. Pipe output keeps the Chat identifier that scripts
/// match against its `Chat:` line; the interactive View names the Agent by its title.
fn background_label(agent: &Agent, console: &Console, chat: uuid::Uuid) -> String {
    if console.is_interactive() {
        readable_background_label(agent, console, chat)
    } else {
        let chat = chat.simple().to_string();
        format!("Chat {}", &chat[..8])
    }
}

/// The title this View last saw for `chat`, with the reading number its rows carry; never a
/// generated identifier.
fn readable_background_label(agent: &Agent, console: &Console, chat: uuid::Uuid) -> String {
    agent
        .cached_title(chat)
        .and_then(|(agent_id, title)| console.agent_name(agent_id, &safe_line(&title)).ok())
        .unwrap_or_else(|| UNLABELED_BACKGROUND_AGENT.into())
}

/// 后台 Chat 的运行结束时显示在状态栏的提示；不写入前台对话。
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

fn background_run_notice(event: &tickloop_agent::runtime::Event) -> Option<BackgroundRunNotice> {
    let (message, necessary) = match &event.kind {
        EventKind::Completed { .. } => ("reply completed".to_owned(), false),
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

/// 后台 Chat 发出权限请求或提问时，管道模式输出的单行提示。
fn background_request_notice(
    event: &tickloop_agent::runtime::Event,
    label: &str,
) -> Option<String> {
    let request = match &event.kind {
        EventKind::PermissionRequested { request } => format!("permission request {}", request.id),
        EventKind::QuestionAsked { request } => format!("question {}", request.id),
        _ => return None,
    };
    Some(format!("{label} · {request}"))
}

/// 管道模式输出后台 Chat 的提示，交互模式保留运行结束提示供状态栏显示；返回是否需要刷新状态栏。
fn announce_background_event(
    console: &Console,
    event: &tickloop_agent::runtime::Event,
    label: &str,
    background_notice: &mut Option<(uuid::Uuid, BackgroundRunNotice)>,
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

/// 维护所有 Chat 的待完成运行与待处理请求，不论事件是否来自前台 Chat。
/// 等待权限或提问的运行一律视为待完成，与 `sync_snapshot` 从快照恢复的口径一致。
fn track_run_requests(
    event: &tickloop_agent::runtime::Event,
    pending: &mut HashMap<uuid::Uuid, Receipt>,
    permissions: &mut HashMap<uuid::Uuid, uuid::Uuid>,
    questions: &mut HashMap<uuid::Uuid, QuestionRequest>,
) {
    let receipt = || Receipt {
        chat_id: event.chat_id,
        loop_id: event.loop_id,
        run_id: event.run_id,
    };
    match &event.kind {
        EventKind::GoalEnded { .. } => {
            let ended_runs = pending
                .values()
                .filter(|receipt| {
                    receipt.chat_id == event.chat_id && receipt.loop_id == event.loop_id
                })
                .map(|receipt| receipt.run_id)
                .collect::<Vec<_>>();
            permissions.retain(|_, run| !ended_runs.contains(run));
            questions.retain(|_, question| {
                question.chat_id != event.chat_id || question.loop_id != event.loop_id
            });
            pending.retain(|_, receipt| {
                receipt.chat_id != event.chat_id || receipt.loop_id != event.loop_id
            });
        }
        EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted => {
            permissions.retain(|_, run| *run != event.run_id);
            questions.retain(|_, question| question.run_id != event.run_id);
            pending.remove(&event.run_id);
        }
        EventKind::InputMerged { .. } | EventKind::InputTakenBack => {
            pending.remove(&event.run_id);
        }
        EventKind::PermissionRequested { request } => {
            pending.entry(event.run_id).or_insert_with(receipt);
            permissions.insert(request.id, request.run_id);
        }
        EventKind::QuestionAsked { request } => {
            pending.entry(event.run_id).or_insert_with(receipt);
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
    chat_id: uuid::Uuid,
    request_id: uuid::Uuid,
    kind: BackgroundWaitKind,
}

/// 非前台 Chat 中等待用户处理的请求；权限请求优先于提问。
fn background_wait(
    selected_chat: Option<uuid::Uuid>,
    pending: &HashMap<uuid::Uuid, Receipt>,
    permissions: &HashMap<uuid::Uuid, uuid::Uuid>,
    questions: &HashMap<uuid::Uuid, QuestionRequest>,
) -> Option<BackgroundWait> {
    let approval = permissions.iter().find_map(|(request_id, run_id)| {
        let chat_id = pending.get(run_id)?.chat_id;
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

/// A run that stopped being waited for because the owner reports its execution Lost.
struct LostRunNotice {
    chat: uuid::Uuid,
    run: uuid::Uuid,
    text: String,
}

fn execution_connection_notice(
    rows: &[crate::agent_access::AgentSummary],
    chat: uuid::Uuid,
    disconnected: bool,
    cached_completed: bool,
    owner_unreachable: bool,
) -> Option<console::ExecutionConnectionNotice> {
    if rows.iter().any(|row| {
        row.chat_id == chat && row.availability == crate::agent_access::ExecutionAvailability::Lost
    }) {
        Some(if cached_completed {
            console::ExecutionConnectionNotice::LostHistory
        } else {
            console::ExecutionConnectionNotice::Lost
        })
    } else if disconnected {
        Some(console::ExecutionConnectionNotice::CachedExecution)
    } else if owner_unreachable {
        Some(console::ExecutionConnectionNotice::CachedView)
    } else {
        None
    }
}

/// Stops waiting for runs whose execution the owner reports Lost; their outcome is unknown and
/// nothing is resent.
fn end_runs_of_lost_executions(
    rows: &[crate::agent_access::AgentSummary],
    pending: &mut HashMap<uuid::Uuid, Receipt>,
    permissions: &mut HashMap<uuid::Uuid, uuid::Uuid>,
    questions: &mut HashMap<uuid::Uuid, QuestionRequest>,
) -> Vec<LostRunNotice> {
    let lost = rows
        .iter()
        .filter(|row| row.availability == crate::agent_access::ExecutionAvailability::Lost)
        .map(|row| row.chat_id)
        .collect::<HashSet<_>>();
    let ended = pending
        .values()
        .filter(|receipt| lost.contains(&receipt.chat_id))
        .cloned()
        .collect::<Vec<_>>();
    ended
        .into_iter()
        .map(|receipt| {
            pending.remove(&receipt.run_id);
            permissions.retain(|_, run| *run != receipt.run_id);
            questions.retain(|_, question| question.run_id != receipt.run_id);
            LostRunNotice {
                chat: receipt.chat_id,
                run: receipt.run_id,
                text: format!(
                    "Run {} has an unknown outcome: its Agent execution was lost; nothing was resent. /resume {} loads its history",
                    receipt.run_id, receipt.chat_id
                ),
            }
        })
        .collect()
}

/// The message the process exits with when input ends while a request is still waiting; it names
/// the owning Chat only when that Chat is in the background.
fn input_closed_while_waiting(
    selected_chat: Option<uuid::Uuid>,
    pending: &HashMap<uuid::Uuid, Receipt>,
    permissions: &HashMap<uuid::Uuid, uuid::Uuid>,
    questions: &HashMap<uuid::Uuid, QuestionRequest>,
) -> Option<String> {
    let is_foreground_permission = |run_id: &uuid::Uuid| {
        pending
            .get(run_id)
            .is_none_or(|receipt| Some(receipt.chat_id) == selected_chat)
    };
    if permissions.values().any(is_foreground_permission) {
        return Some(
            "input closed while waiting for permission; no pending operation was approved".into(),
        );
    }
    let wait = background_wait(selected_chat, pending, permissions, questions);
    if let Some(wait) = wait.filter(|wait| wait.kind == BackgroundWaitKind::Approval) {
        return Some(format!(
            "input closed while background Chat {} was waiting for permission request {}; no pending operation was approved",
            wait.chat_id, wait.request_id
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
            wait.chat_id, wait.request_id
        )
    })
}

#[derive(Debug, Default, Eq, PartialEq)]
struct BackgroundRecovery {
    run_notices: Vec<BackgroundRunNotice>,
    request_notices: Vec<String>,
    failed: bool,
}

/// 事件流滞后后按快照校正一个后台 Chat 的待完成运行与请求，并补回丢失的结束与请求提示。
fn recover_background_chat(
    snapshot: &tickloop_agent::runtime::ChatSnapshot,
    label: &str,
    pending: &mut HashMap<uuid::Uuid, Receipt>,
    permissions: &mut HashMap<uuid::Uuid, uuid::Uuid>,
    questions: &mut HashMap<uuid::Uuid, QuestionRequest>,
) -> BackgroundRecovery {
    use tickloop_agent::runtime::LoopState;
    let runs_before = pending
        .values()
        .filter(|receipt| receipt.chat_id == snapshot.id)
        .cloned()
        .collect::<Vec<_>>();
    let known_permissions = permissions.keys().copied().collect::<HashSet<_>>();
    let known_questions = questions.keys().copied().collect::<HashSet<_>>();
    sync_snapshot(snapshot, pending, permissions, questions);
    let mut recovery = BackgroundRecovery::default();
    let mut ended_loops = HashSet::new();
    for receipt in runs_before {
        if pending.contains_key(&receipt.run_id) || !ended_loops.insert(receipt.loop_id) {
            continue;
        }
        let state = snapshot
            .loops
            .iter()
            .find(|goal| goal.id == receipt.loop_id)
            .map(|goal| goal.state.clone());
        let outcome = match state {
            Some(LoopState::Failed) => {
                recovery.failed = true;
                "failed"
            }
            Some(LoopState::Interrupted | LoopState::Stopped) => "interrupted",
            Some(
                LoopState::Running
                | LoopState::Queued
                | LoopState::WaitingPermission
                | LoopState::WaitingQuestion,
            ) => continue,
            _ => "reply completed",
        };
        recovery.run_notices.push(BackgroundRunNotice {
            message: outcome.into(),
            compact_message: None,
            necessary: outcome == "failed",
        });
    }
    for request in &snapshot.permissions {
        if !known_permissions.contains(&request.id) {
            recovery
                .request_notices
                .push(format!("{label} · permission request {}", request.id));
        }
    }
    for request in &snapshot.questions {
        if !known_questions.contains(&request.id) {
            recovery
                .request_notices
                .push(format!("{label} · question {}", request.id));
        }
    }
    recovery
}

fn task_status_items(snapshot: &ShellTaskSnapshot) -> Vec<StatusItem> {
    snapshot
        .tasks
        .iter()
        .filter(|task| task.was_background && !task.state.is_terminal())
        .enumerate()
        .map(|(index, task)| {
            let (state, role) = match task.state {
                ShellTaskState::Queued => ("queued", Role::MutedText),
                ShellTaskState::Running | ShellTaskState::Background => {
                    ("running", Role::MutedText)
                }
                ShellTaskState::Cancelling => ("stopping", Role::Warning),
                ShellTaskState::Completed | ShellTaskState::Failed | ShellTaskState::Unknown => {
                    unreachable!("terminal tasks are filtered above")
                }
            };
            StatusItem {
                id: format!("{}.", index + 1),
                task_id: Some(task.id),
                can_stop: task.capabilities.can_stop,
                title: safe_text(&task.command).replace('\n', " "),
                state,
                role,
            }
        })
        .collect()
}

fn todo_status_items(snapshot: &ChatSnapshot, current_loop: Option<uuid::Uuid>) -> Vec<StatusItem> {
    snapshot
        .progress
        .iter()
        .find(|progress| Some(progress.loop_id) == current_loop)
        .map(|progress| {
            progress
                .todos
                .iter()
                .enumerate()
                .map(|(index, todo)| {
                    let (state, role) = match todo.status {
                        TodoStatus::Pending => ("pending", Role::MutedText),
                        TodoStatus::InProgress => ("in progress", Role::Warning),
                        TodoStatus::Completed => ("completed", Role::Success),
                    };
                    StatusItem {
                        id: format!("{}.", index + 1),
                        task_id: None,
                        can_stop: false,
                        title: safe_text(&todo.content),
                        state,
                        role,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Like Claude Code's spinner, the running goal's in-progress todo names the current activity in
/// its `activeForm`; otherwise the activity is the generic "Working".
fn working_activity(snapshot: &ChatSnapshot, running_loop: Option<LoopId>) -> String {
    running_loop
        .and_then(|loop_id| {
            snapshot
                .progress
                .iter()
                .find(|progress| progress.loop_id == loop_id)
        })
        .and_then(|progress| progress.active_form())
        .map_or_else(
            || WORKING_ACTIVITY.to_owned(),
            |active_form| safe_text(active_form).replace('\n', " "),
        )
}

fn context_usage(snapshot: &ChatSnapshot) -> Option<(u64, usize)> {
    let limit = snapshot
        .effective_context_window_tokens()
        .filter(|limit| *limit > 0)?;
    let matching_entries = snapshot
        .request_usage
        .entries()
        .iter()
        .rev()
        .filter(|entry| {
            entry.kind == RequestKind::Conversation && entry.model == snapshot.model.identity()
        })
        .collect::<Vec<_>>();
    let reported_usage = matching_entries
        .iter()
        .find_map(|entry| entry.context_input_tokens());
    let used = if matching_entries.is_empty() {
        0
    } else {
        reported_usage
            .or_else(|| (matching_entries[0].outcome == RequestOutcome::InFlight).then_some(0))?
    };
    Some((used, limit))
}

fn model_retry_notice(
    kind: RequestKind,
    error: &str,
    attempt: usize,
    max_retries: usize,
    delay_ms: u64,
) -> String {
    let request = match kind {
        RequestKind::PermissionCheck => "Auto review",
        RequestKind::WebFetch => "WebFetch",
        _ => "Model request",
    };
    format!(
        "{request}: retrying in {:.1}s (attempt {}/{}) · {}",
        delay_ms as f64 / 1000.0,
        attempt + 1,
        max_retries + 1,
        safe_text(error),
    )
}

fn usage_text(usage: &Usage) -> String {
    let mut values = Vec::new();
    if let Some(value) = usage.input_tokens {
        values.push(format!("input tokens: {value}"));
    }
    if let Some(value) = usage.output_tokens {
        values.push(format!("output tokens: {value}"));
    }
    if values.is_empty() {
        String::new()
    } else {
        format!(" · {}", values.join(", "))
    }
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

fn selected_loop(snapshot: &ChatSnapshot) -> Option<uuid::Uuid> {
    (snapshot.loops.len() == 1).then(|| snapshot.loops[0].id)
}

fn creation_notice(projection: &crate::agent_console::Projection) -> Option<String> {
    projection.unconfirmed_dispatch.map(|command_id| {
        format!("Creation {command_id} unknown; inspect Agents, then /acknowledge-create {command_id} to allow another submission without replay")
    })
}

fn creation_attach_unavailable(error: &AgentError) -> String {
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

fn agent_overview(projection: &crate::agent_console::Projection) -> console::AgentOverview {
    console::AgentOverview {
        rows: agent_tree_rows(&projection.rows, true),
        dispatch_directory: projection.dispatch_directory.clone(),
        created: projection.created.map(|execution| execution.agent_id),
        visible: projection.visible.map(|execution| execution.agent_id),
    }
}

/// Human status text for an Agent row; the Loop state names are internal.
fn agent_status_label(row: &crate::agent_access::AgentSummary) -> &'static str {
    use crate::agent_access::ExecutionAvailability;
    use tickloop_agent::runtime::LoopState;
    if row.availability == ExecutionAvailability::Lost {
        return "Lost · outcome unknown";
    }
    if row.has_work == Some(false) && row.waiting_permissions + row.waiting_questions == 0 {
        return "Empty";
    }
    match row.state {
        LoopState::Running => "Running",
        LoopState::Queued => "Queued",
        LoopState::WaitingPermission => "Needs approval",
        LoopState::WaitingQuestion => "Needs answer",
        LoopState::WaitingPlan => "Plan review",
        LoopState::WaitingTrigger => "Waiting · trigger",
        LoopState::Idle if row.has_work.is_none() => "Idle · work unknown",
        LoopState::Idle => "Idle",
        LoopState::Completed => "Completed",
        LoopState::Failed => "Failed",
        LoopState::Interrupted => "Interrupted",
        LoopState::Stopped => "Stopped",
        LoopState::Expired => "Expired",
    }
}

fn agent_tree_rows(
    rows: &[crate::agent_access::AgentSummary],
    aggregate: bool,
) -> Vec<console::AgentTreeRow> {
    fn append(
        rows: &[crate::agent_access::AgentSummary],
        row: &crate::agent_access::AgentSummary,
        depth: usize,
        seen: &mut HashSet<crate::agent_access::ExecutionRef>,
        output: &mut Vec<console::AgentTreeRow>,
        aggregate: bool,
    ) -> (usize, usize) {
        if !seen.insert(row.execution) {
            return (0, 0);
        }
        let position = output.len();
        let group = crate::agent_console::AttentionGroup::of(row);
        output.push(console::AgentTreeRow {
            agent_id: row.execution.agent_id,
            parent_id: row
                .parent
                .filter(|parent| rows.iter().any(|candidate| candidate.execution == *parent))
                .map(|parent| parent.agent_id),
            depth,
            title: safe_line(&row.title),
            status: agent_status_label(row).into(),
            project: row.workdir.to_string_lossy().into_owned(),
            workspace: row.workspace.clone(),
            approvals: row.waiting_permissions,
            questions: row.waiting_questions,
            group,
            glyph: console::status_glyph(
                group,
                row.state == tickloop_agent::runtime::LoopState::Completed,
            ),
            preview: row.preview.as_deref().map(safe_line),
            updated_at: row.updated_at,
        });
        let mut counts = (row.waiting_permissions, row.waiting_questions);
        for child in rows
            .iter()
            .filter(|candidate| candidate.parent == Some(row.execution))
        {
            let child_counts = append(rows, child, depth + 1, seen, output, aggregate);
            counts.0 += child_counts.0;
            counts.1 += child_counts.1;
        }
        if aggregate && depth == 0 {
            output[position].approvals = counts.0;
            output[position].questions = counts.1;
        }
        counts
    }
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for row in rows.iter().filter(|row| {
        row.parent
            .is_none_or(|parent| !rows.iter().any(|candidate| candidate.execution == parent))
    }) {
        append(rows, row, 0, &mut seen, &mut result, aggregate);
    }
    for row in rows {
        append(rows, row, 0, &mut seen, &mut result, aggregate);
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
    use tick_connect::herdr::{HerdrPort, SocketHerdr};
    for _ in 0..2 {
        if let Destination::Unavailable(record) = &effect.destination {
            console.set_status(&format!(
                "Execution interrupted; open saved history explicitly{}",
                record
                    .as_ref()
                    .map(|record| format!(" with /resume {}", record.chat_id))
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
                menus.dismiss(console)?;
                console.set_global_overview(None)?;
                let restored = console.activate_agent(chat)?;
                if !restored {
                    workflows::write_public_history(console, &snapshot)?;
                } else if snapshot.last_sequence > sequences.get(&chat).copied().unwrap_or(0) {
                    let mut presentation = ToolPresentation::new(!console.is_interactive());
                    let observation = restore_view(
                        console,
                        agent,
                        &mut presentation,
                        chat,
                        sequences.get(&chat).copied().unwrap_or(0),
                        reply_lines,
                    )
                    .await?;
                    snapshot = observation.snapshot;
                }
                console.set_input_context(
                    Some(chat),
                    &snapshot.workdir,
                    &snapshot.additional_directories,
                )?;
                console.set_visible_delegation(snapshot.delegation.is_some())?;
                let family = global.family(effect.target);
                console.set_agent_family(
                    if family.len() > 1 {
                        agent_tree_rows(&family, false)
                    } else {
                        Vec::new()
                    },
                    Some(effect.target.agent_id),
                )?;
                if !global.complete(&effect) {
                    return Ok(None);
                }
                agent
                    .display_chat(Some(chat))
                    .await
                    .map_err(|error| error.to_string())?;
                return Ok(Some(snapshot));
            }
            Destination::Focus(destination) => {
                use std::os::unix::fs::MetadataExt;
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
                if host.host_instance != format!("herdr:{}:{}", metadata.dev(), metadata.ino()) {
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
                let current: tick_connect::herdr::Pane =
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
    use std::sync::Arc;

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
            serde_json::json!({"source":{"sender_relationship":"child","sender_label":"Report check","is_result":false},"message":"Which year should I use?"})
        );
        let mut snapshot:ChatSnapshot=serde_json::from_value(serde_json::json!({"id":chat,"workdir":"/work","messages":[{"role":"user","content":[{"type":"text","text":text}]}],"loops":[]})).unwrap();
        snapshot.message_ids.insert(0, source);
        snapshot.message_runs.insert(0, run);
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
    #[test]
    fn explicit_provider_yaml_selects_declared_default_model() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("deepseek-api.yaml");
        std::fs::write(
            &path,
            include_str!("../examples/providers/deepseek-api.yaml"),
        )
        .unwrap();
        let selected = super::read_config(&path).unwrap();
        assert_eq!(selected.provider_id, "deepseek-api");
        assert_eq!(selected.model_id, "deepseek-flash");
        std::fs::write(&path, "id: deepseek-api\nmodels:\n  - id: deepseek-flash-messages\n  - id: deepseek-flash-responses\n").unwrap();
        assert_eq!(
            super::read_config(&path).unwrap().model_id,
            "deepseek-flash"
        );
        std::fs::write(
            &path,
            "id: deepseek-api\ndefault_model: deepseek-flash-messages\n",
        )
        .unwrap();
        let fixed = super::read_config(&path).unwrap();
        assert_eq!(fixed.model_id, "deepseek-flash-messages");
        assert_eq!(
            fixed.adapter(),
            crate::model::cards::Adapter::DeepseekMessages
        );
        assert_eq!(fixed.base_url, "https://api.deepseek.com/anthropic");
    }

    use super::InputEvent;
    use super::{
        Agent, BranchRequest, CommandTiming, Console, EventKind, Menus, Renderer, TUI_SYNTAX,
        Workflow, branch_failure_message, branch_name, command_timing, help_text,
        is_branch_command, is_rewind_command, is_subscription_view_command,
        is_task_surface_command, open_rewind, safe_line, should_dismiss_menu_before_dispatch,
        should_pause_input_for_summary, switch_to_saved_branch, task_status_items,
        test_support as support, tui_renderer_request,
    };
    use super::{
        BackgroundRecovery, BackgroundRunNotice, BackgroundWaitKind, Receipt,
        UNLABELED_BACKGROUND_AGENT, background_wait, context_usage, input_closed_while_waiting,
        observe_user_shell_record, readable_background_label, recover_background_chat,
        todo_status_items, track_run_requests, working_activity,
    };
    use std::{
        collections::{HashMap, HashSet},
        path::PathBuf,
    };
    use tickloop_agent::permissions::{Decision, PermissionRule};
    use tickloop_agent::runtime::{
        RequestKind, RequestOutcome, RequestOwner, ShellTask, ShellTaskCapabilities,
        ShellTaskOrigin, ShellTaskOwner, ShellTaskSnapshot, ShellTaskState,
    };
    use uuid::Uuid;

    #[test]
    fn status_panel_counts_only_active_background_shell_tasks() {
        let chat_id = Uuid::new_v4();
        let tasks = [
            (false, ShellTaskState::Running),
            (true, ShellTaskState::Queued),
            (true, ShellTaskState::Background),
            (true, ShellTaskState::Cancelling),
            (true, ShellTaskState::Completed),
            (true, ShellTaskState::Failed),
            (true, ShellTaskState::Unknown),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (was_background, state))| ShellTask {
            id: Uuid::from_u128(index as u128 + 1),
            chat_id,
            origin: ShellTaskOrigin::User,
            owner: ShellTaskOwner::User { chat_id },
            command: format!("command-{index}"),
            cwd: PathBuf::from("/tmp"),
            state,
            was_background,
            pid: None,
            exit_code: None,
            signal: None,
            reason: None,
            output_bytes: 0,
            capabilities: ShellTaskCapabilities::default(),
        })
        .collect();
        let snapshot = ShellTaskSnapshot {
            chat_id,
            live_revision: 1,
            tasks,
        };

        let status_items = task_status_items(&snapshot);
        assert_eq!(status_items.len(), 3);
        assert_eq!(status_items[0].title, "command-1");
        assert_eq!(status_items[1].title, "command-2");
        assert_eq!(status_items[2].title, "command-3");
    }

    #[tokio::test]
    async fn todos_show_their_content_and_the_running_goal_shows_its_active_form() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(support::ScriptedModel::new(vec![
            support::call(
                "todos",
                tickloop_agent::runtime::TODO_WRITE_TOOL_NAME,
                serde_json::json!({"todos": [
                    {"content": "Read the notes", "status": "completed", "activeForm": "Reading the notes"},
                    {"content": "Draft the\nreport", "status": "in_progress", "activeForm": "Drafting the\nreport"},
                    {"content": "Check the figures", "status": "pending", "activeForm": "Checking the figures"},
                ]}),
            ),
            support::text("Drafting."),
        ]));
        let agent = Agent::new_embedded(support::config(), model);
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        let receipt = agent
            .submit(chat, None, "Draft the report".into())
            .await
            .unwrap();
        loop {
            match support::next_event(&mut events).await.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("todo fixture failed: {error}"),
                _ => {}
            }
        }
        let mut snapshot = agent.snapshot(chat).await.unwrap();

        let todos = todo_status_items(&snapshot, Some(receipt.loop_id));
        assert_eq!(
            todos
                .iter()
                .map(|item| (item.title.as_str(), item.state))
                .collect::<Vec<_>>(),
            [
                ("Read the notes", "completed"),
                ("Draft the\nreport", "in progress"),
                ("Check the figures", "pending"),
            ]
        );
        assert_eq!(
            working_activity(&snapshot, Some(receipt.loop_id)),
            "Drafting the report"
        );
        assert_eq!(working_activity(&snapshot, None), "Working");
        assert_eq!(working_activity(&snapshot, Some(Uuid::new_v4())), "Working");
        snapshot.progress[0].todos[1].status = tickloop_agent::runtime::TodoStatus::Completed;
        assert_eq!(
            working_activity(&snapshot, Some(receipt.loop_id)),
            "Working"
        );
    }

    #[tokio::test]
    async fn selected_loop_todos_and_activity_follow_that_loop_when_another_goal_is_newer() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(support::ScriptedModel::new(vec![
            support::call(
                "first-todos",
                tickloop_agent::runtime::TODO_WRITE_TOOL_NAME,
                serde_json::json!({"todos":[
                    {"content":"Draft the first report","status":"in_progress","activeForm":"Drafting the first report"},
                    {"content":"Review the first report","status":"pending","activeForm":"Reviewing the first report"},
                ]}),
            ),
            support::text("First goal recorded."),
            support::call(
                "second-todos",
                tickloop_agent::runtime::TODO_WRITE_TOOL_NAME,
                serde_json::json!({"todos":[
                    {"content":"Check the second dataset","status":"completed","activeForm":"Checking the second dataset"},
                    {"content":"Summarize the second dataset","status":"in_progress","activeForm":"Summarizing the second dataset"},
                ]}),
            ),
            support::text("Second goal recorded."),
        ]));
        let agent = Agent::new_embedded(support::config(), model);
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        let mut goals = Vec::new();
        for input in ["Draft the first report", "Summarize the second dataset"] {
            let receipt = agent.submit(chat, None, input.into()).await.unwrap();
            loop {
                let event = support::next_event(&mut events).await;
                if event.run_id != receipt.run_id {
                    continue;
                }
                match event.kind {
                    EventKind::Completed { .. } => break,
                    EventKind::Failed(error) => panic!("todo fixture failed: {error}"),
                    _ => {}
                }
            }
            goals.push(receipt.loop_id);
        }
        assert_ne!(goals[0], goals[1]);
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.progress.len(), 2);
        assert_eq!(snapshot.progress.last().unwrap().loop_id, goals[1]);
        for (goal, expected, activity) in [
            (
                goals[0],
                vec![
                    ("Draft the first report", "in progress"),
                    ("Review the first report", "pending"),
                ],
                "Drafting the first report",
            ),
            (
                goals[1],
                vec![
                    ("Check the second dataset", "completed"),
                    ("Summarize the second dataset", "in progress"),
                ],
                "Summarizing the second dataset",
            ),
        ] {
            let todos = todo_status_items(&snapshot, Some(goal));
            assert_eq!(
                todos
                    .iter()
                    .map(|item| (item.title.as_str(), item.state))
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(working_activity(&snapshot, Some(goal)), activity);
        }
        assert!(todo_status_items(&snapshot, None).is_empty());
        assert_eq!(working_activity(&snapshot, None), "Working");
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn context_starts_at_zero_and_uses_the_effective_model_window() {
        let agent = Agent::new_embedded(
            support::config(),
            Arc::new(support::ScriptedModel::new(Vec::new())),
        );
        let chat_id = agent.create_chat(PathBuf::from("/tmp")).await.unwrap();
        let mut snapshot = agent.snapshot(chat_id).await.unwrap();
        snapshot.model = tickloop_agent::model::cards::ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash-messages")
            .unwrap();
        assert_eq!(context_usage(&snapshot), Some((0, 1_000_000)));

        let request = snapshot
            .request_usage
            .start(
                RequestOwner {
                    chat_id,
                    loop_id: Some(Uuid::new_v4()),
                    run_id: Some(Uuid::new_v4()),
                },
                RequestKind::Conversation,
                snapshot.model.identity(),
            )
            .unwrap();
        assert_eq!(context_usage(&snapshot), Some((0, 1_000_000)));
        let mut without_report = snapshot.clone();
        without_report
            .request_usage
            .finish(request, RequestOutcome::Accepted)
            .unwrap();
        assert_eq!(context_usage(&without_report), None);
        snapshot
            .request_usage
            .observe(
                request,
                tickloop_agent::model::Usage {
                    input_tokens: Some(10_000),
                    output_tokens: None,
                    cache_read_input_tokens: Some(2_000),
                    cache_write_input_tokens: Some(500),
                },
            )
            .unwrap();
        assert_eq!(context_usage(&snapshot), Some((12_500, 1_000_000)));
        snapshot
            .request_usage
            .finish(request, RequestOutcome::Accepted)
            .unwrap();
        snapshot
            .request_usage
            .start(
                RequestOwner {
                    chat_id,
                    loop_id: Some(Uuid::new_v4()),
                    run_id: Some(Uuid::new_v4()),
                },
                RequestKind::Conversation,
                snapshot.model.identity(),
            )
            .unwrap();
        assert_eq!(context_usage(&snapshot), Some((12_500, 1_000_000)));
    }

    fn user_shell_record(
        chat_id: Uuid,
        id: Uuid,
        state: tickloop_agent::runtime::UserShellState,
        starts_in_background: bool,
    ) -> tickloop_agent::runtime::UserShellRecord {
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
        use tickloop_agent::runtime::UserShellState;
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
        use tickloop_agent::runtime::UserShellState;
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
            "/settings",
            "/interrupt",
            "/diff",
            "/tasks",
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
        for command in [
            "/compact",
            "/cwd elsewhere",
            "/add-dir ../shared",
            "/search-config search.json",
            "/loops schedule 00000000-0000-0000-0000-000000000000 60 check",
        ] {
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
            text: "/branch summary-busy".into(),
            file_references: Vec::new(),
            images: Vec::new(),
        }));
        assert!(!should_pause_input_for_summary(&InputEvent::Line(
            "/branch".into()
        )));
        assert!(should_pause_input_for_summary(&InputEvent::Prompt {
            text: "continue after summary".into(),
            file_references: Vec::new(),
            images: Vec::new(),
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
            support::config(),
            Arc::new(support::ScriptedModel::new(vec![support::text(
                "source answer",
            )])),
        )
        .unwrap();
        let source_chat_id = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        let source = agent
            .submit(source_chat_id, None, "source prompt".into())
            .await
            .unwrap();
        loop {
            match support::next_event(&mut events).await.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("source fixture failed: {error}"),
                _ => {}
            }
        }
        let outcome = agent
            .branch_chat(BranchRequest {
                request_id: Uuid::new_v4(),
                source_chat_id,
                source_loop_id: Some(source.loop_id),
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
        assert!(failure.contains(&format!("Branch {child_chat_id} was saved")));
        assert!(failure.contains("renderer\\nfailed\\u{1b}[2J"));
        assert!(failure.contains("editor\\ncleanup\\u{1b}[3J"));
        assert!(failure.contains("Use /resume to find it"));
        assert!(failure.contains(&format!("/resume {child_chat_id}")));
        assert!(!failure.contains('\x1b'));
        assert_eq!(agent.list_chats().await.len(), 2);
        assert_eq!(
            serde_json::to_value(agent.snapshot(child_chat_id).await.unwrap()).unwrap(),
            durable_child
        );
        agent.shutdown().await;
        drop(agent);

        let reopened = Agent::open_embedded(
            archive,
            support::config(),
            Arc::new(support::ScriptedModel::new(Vec::new())),
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
            picker
                .entries()
                .iter()
                .all(|entry| !entry.plain_row().contains(&child_chat_id.to_string())),
            "picker rows must not show Chat IDs"
        );
        let effect = workflow
            .choose(&reopened, &console, Some(child_index))
            .await
            .unwrap();
        // 子 Chat 带着分出点之前创建的唯一 Loop，选择器随之选中它。
        assert_eq!(
            effect.selection,
            Some((child_chat_id, Some(source.loop_id)))
        );
        // Branch 的创建请求与复制种子只在创建它的进程内有效，恢复时由元数据只还原来源与名称。
        let mut restored_child = durable_child.clone();
        restored_child["branch_origin"]["creation"] = serde_json::Value::Null;
        assert_eq!(
            serde_json::to_value(effect.restored.unwrap()).unwrap(),
            restored_child
        );
        reopened.shutdown().await;
    }

    #[test]
    fn task_command_has_one_canonical_dispatch_name() {
        {
            let command = "/tasks";
            assert!(is_task_surface_command(command), "{command}");
            assert_eq!(
                command_timing(command),
                CommandTiming::Immediate,
                "{command}"
            );
        }
        assert!(!is_task_surface_command("/tasks extra"));
        assert!(!is_task_surface_command("/bash"));
        assert!(!is_task_surface_command("/bashes"));
        assert!(!is_task_surface_command("/commands"));
    }

    #[test]
    fn subscription_view_dispatches_immediately_and_keeps_its_view_open() {
        {
            let command = "/subscriptions";
            assert!(is_subscription_view_command(command), "{command}");
            assert_eq!(
                command_timing(command),
                CommandTiming::Immediate,
                "{command}"
            );
            assert!(
                !should_dismiss_menu_before_dispatch(command),
                "{command} opens a view of its own"
            );
        }
        assert!(!is_subscription_view_command("/subscriptions extra"));
        assert!(!is_subscription_view_command("/subscribe"));
        assert!(!is_subscription_view_command("/subs"));
        assert!(help_text().contains("/subscriptions"));
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
        let model = Arc::new(support::ScriptedModel::new(vec![
            support::text("Recorded a real prompt"),
            support::call(
                "write",
                "Write",
                serde_json::json!({
                    "file_path": "approval-required.txt", "content": "requires approval"
                }),
            ),
        ]));
        let agent = Agent::new_embedded(support::config(), model.clone());
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
        open_rewind(&agent, &console, &mut menus, Some(chat), false)
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
            .submit(chat, None, "Record this prompt".into())
            .await
            .unwrap();
        loop {
            match support::next_event(&mut events).await.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("rewind entry fixture failed: {error}"),
                _ => {}
            }
        }
        let before = agent.snapshot(chat).await.unwrap();
        open_rewind(&agent, &console, &mut menus, Some(chat), true)
            .await
            .unwrap();
        assert!(menus.id().is_none());
        {
            let command = "/rewind";
            assert!(is_rewind_command(command));
            open_rewind(&agent, &console, &mut menus, Some(chat), false)
                .await
                .unwrap();
            assert!(
                menus.id().is_some(),
                "{command} did not open the recorded selector"
            );
            let opened = menus.id();
            open_rewind(&agent, &console, &mut menus, Some(chat), true)
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
            .submit(chat, None, "Request a file write".into())
            .await
            .unwrap();
        loop {
            match support::next_event(&mut events).await.kind {
                EventKind::PermissionRequested { .. } => break,
                EventKind::Failed(error) => panic!("permission fixture failed: {error}"),
                _ => {}
            }
        }
        let pending = agent.snapshot(chat).await.unwrap();
        open_rewind(&agent, &console, &mut menus, Some(chat), false)
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
    async fn lagged_recovery_restores_background_requests_and_reports_runs_that_ended() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(support::ScriptedModel::new(vec![
            support::call(
                "write",
                "Write",
                serde_json::json!({"file_path": "background.txt", "content": "needs approval"}),
            ),
            support::text("Wrote the background file"),
        ]));
        let agent = Agent::new_embedded(support::config(), model);
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
            .submit(background_chat, None, "Write in the background".into())
            .await
            .unwrap();
        let request = loop {
            match support::next_event(&mut events).await.kind {
                EventKind::PermissionRequested { request } => break request,
                EventKind::Failed(error) => panic!("permission fixture failed: {error}"),
                _ => {}
            }
        };
        let (console, _) = Console::start(false).unwrap();
        assert_eq!(
            readable_background_label(&agent, &console, Uuid::new_v4()),
            UNLABELED_BACKGROUND_AGENT,
            "an Agent this View has not seen is not given an invented name"
        );
        agent.snapshot(background_chat).await.unwrap();
        let label = readable_background_label(&agent, &console, background_chat);
        assert_eq!(label, "Agent Write in the background");

        // 滞后丢失了权限请求事件：终端只知道提交回执。
        let mut pending = HashMap::from([(receipt.run_id, receipt.clone())]);
        let mut permissions = HashMap::new();
        let mut questions = HashMap::new();
        let snapshot = agent.snapshot(background_chat).await.unwrap();
        let recovery = recover_background_chat(
            &snapshot,
            &label,
            &mut pending,
            &mut permissions,
            &mut questions,
        );
        assert_eq!(
            recovery,
            BackgroundRecovery {
                run_notices: Vec::new(),
                request_notices: vec![format!("{label} · permission request {}", request.id)],
                failed: false,
            }
        );
        assert!(pending.contains_key(&receipt.run_id));
        assert_eq!(permissions.get(&request.id), Some(&receipt.run_id));
        let wait = background_wait(foreground_chat, &pending, &permissions, &questions).unwrap();
        assert_eq!(wait.kind, BackgroundWaitKind::Approval);
        assert_eq!(wait.chat_id, background_chat);
        assert_eq!(wait.request_id, request.id);
        let closed =
            input_closed_while_waiting(foreground_chat, &pending, &permissions, &questions)
                .unwrap();
        assert!(
            closed.contains(&format!("background Chat {background_chat}"))
                && closed.contains(&request.id.to_string()),
            "{closed}"
        );
        assert!(
            background_wait(Some(background_chat), &pending, &permissions, &questions).is_none()
        );

        agent
            .answer_permission(background_chat, request.id, true)
            .await
            .unwrap();
        loop {
            match support::next_event(&mut events).await.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("approved fixture failed: {error}"),
                _ => {}
            }
        }
        // 滞后又丢失了运行结束事件：残留的请求与待完成运行按快照清理并补回结束提示。
        let snapshot = agent.snapshot(background_chat).await.unwrap();
        let recovery = recover_background_chat(
            &snapshot,
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
            input_closed_while_waiting(foreground_chat, &pending, &permissions, &questions)
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
        let merged = |sequence: u64, run: u128, into: Uuid| tickloop_agent::runtime::Event {
            sequence,
            chat_id: chat,
            loop_id: Uuid::from_u128(3),
            run_id: Uuid::from_u128(run),
            kind: EventKind::InputMerged { into_run_id: into },
        };
        let accept = |echoes: &mut MergedEchoes| {
            echoes.accepted(Uuid::from_u128(10), Some(alpha));
            echoes.accepted(Uuid::from_u128(11), Some(bravo));
            echoes.accepted(Uuid::from_u128(12), Some(charlie));
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
            &tickloop_agent::runtime::Event {
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
        let merged = |sequence: u64, run: u128| tickloop_agent::runtime::Event {
            sequence,
            chat_id: Uuid::from_u128(1),
            loop_id: Uuid::from_u128(3),
            run_id: Uuid::from_u128(run),
            kind: EventKind::InputMerged { into_run_id: into },
        };
        let mut echoes = MergedEchoes::default();
        echoes.accepted(Uuid::from_u128(10), Some(alpha));
        echoes.accepted(Uuid::from_u128(11), Some(bravo));
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
        live.accepted(Uuid::from_u128(10), Some(alpha));
        live.accepted(Uuid::from_u128(11), Some(bravo));
        live.observe(&merged(20, 10), 19);
        assert_eq!(
            live.observe(&merged(21, 11), 20),
            Some((alpha, bravo)),
            "the same merges, both live, still read as one block"
        );
    }

    #[test]
    fn background_run_events_clear_pending_work_even_when_only_merged_or_taken_back() {
        let chat_id = Uuid::from_u128(1);
        let receipt = |run: u128| Receipt {
            chat_id,
            loop_id: Uuid::from_u128(2),
            run_id: Uuid::from_u128(run),
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
                    into_run_id: Uuid::from_u128(5),
                },
            ),
            (4, EventKind::InputTakenBack),
        ] {
            let event = tickloop_agent::runtime::Event {
                sequence: 1,
                chat_id,
                loop_id: Uuid::from_u128(2),
                run_id: Uuid::from_u128(run),
                kind,
            };
            track_run_requests(&event, &mut pending, &mut permissions, &mut questions);
        }
        assert!(pending.is_empty());
    }
}

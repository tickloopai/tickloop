use super::execution::ConsoleExecution as Agent;
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

use uuid::Uuid;

use super::{
    agent_messaging::{self, AgentColors, AgentInput, AgentMessage},
    console::{Console, ConversationPiece, Menu},
    conversation,
    conversation_picker::{
        ConversationEntry, ConversationPicker, ConversationPickerPurpose, compact_line,
    },
    directed_summary,
    display::{DisplayProfile, Glyph},
    image_cache::ImageLinks,
    presentation, safe_text,
};
use tickloop_harness::protocol::{
    BranchPublicItem, BranchPublicSource, ChatSnapshot, CommandSnapshot, CommandState, Content,
    EventKind, ExecutionMode, FOLDED_TICK_REPLY_CHARACTERS, InputContext, LocalMessage, LoopState,
    Receipt, Role, SUBAGENT_TOOL_NAME, SavedChat, ToolResult, UserShellRecord, UserShellState,
};

const COMMAND_OUTPUT_PAGE_BYTES: usize = 16 * 1024;
const PROCESS_LOCAL_TASKS_NOTE: &str =
    "Shell tasks and their output are available only in this process; a restart clears them.";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanApprovalChoice {
    Auto,
    Ask,
    FullAccess,
    KeepPlanning,
}

/// "Keep planning" is always the last choice; the plan dialog takes its feedback there.
pub(super) const PLAN_APPROVAL_CHOICES: [PlanApprovalChoice; 4] = [
    PlanApprovalChoice::Auto,
    PlanApprovalChoice::Ask,
    PlanApprovalChoice::FullAccess,
    PlanApprovalChoice::KeepPlanning,
];

impl PlanApprovalChoice {
    /// The execution mode the approval starts with; keeping planning starts none.
    fn mode(self) -> Option<ExecutionMode> {
        match self {
            Self::Auto => Some(ExecutionMode::Auto),
            Self::Ask => Some(ExecutionMode::AcceptEdits),
            Self::FullAccess => Some(ExecutionMode::FullAccess),
            Self::KeepPlanning => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "Approve and start · Auto",
            Self::Ask => "Approve and start · Ask",
            Self::FullAccess => "Approve and start · Full Access",
            Self::KeepPlanning => "No, keep planning",
        }
    }
}

pub(super) enum Workflow {
    Chats {
        choices: Vec<tickloop_harness::protocol::ChatId>,
        as_new_chat: bool,
    },
    Commands {
        chat: Uuid,
        choices: Vec<CommandChoice>,
    },
    Command {
        chat: Uuid,
        operation: CommandChoice,
        actions: Vec<CommandAction>,
    },
    Plan {
        chat: Uuid,
        version: u64,
        request: Option<Uuid>,
    },
    PlanView {
        chat: Uuid,
    },
}

#[derive(Clone, Copy)]
pub(super) enum CommandChoice {
    Agent(Uuid),
    User(Uuid),
}

impl CommandChoice {
    fn origin(self) -> &'static str {
        match self {
            Self::Agent(_) => "Agent",
            Self::User(_) => "User",
        }
    }
}

pub(super) enum CommandAction {
    Output(u64),
    Background,
    Stop,
    Back,
}

#[derive(Default)]
pub(super) struct WorkflowEffect {
    pub(super) next: Option<(Menu, Workflow)>,
    pub(super) selection: Option<Uuid>,
    pub(super) receipt: Option<Receipt>,
    pub(super) restored: Option<ChatSnapshot>,
}

impl Workflow {
    pub(super) fn chat(&self) -> Option<Uuid> {
        match self {
            Self::Chats { .. } => None,
            Self::Commands { chat, .. }
            | Self::Command { chat, .. }
            | Self::Plan { chat, .. }
            | Self::PlanView { chat } => Some(*chat),
        }
    }

    /// The saved-conversation picker: this project's saved Chats except the one in front,
    /// newest activity first, each with its title and its known activity age, git branch and
    /// Branch source. `as_new_chat` copies the chosen conversation into a new Chat
    /// (`--fork-session`). Entry `i` of the picker is `choices[i]`.
    pub(super) async fn chats(
        agent: &Agent,
        workdir: &std::path::Path,
        current_chat: Option<Uuid>,
        as_new_chat: bool,
    ) -> Result<(ConversationPicker, Self), String> {
        let saved = agent
            .saved_chats(workdir)
            .await
            .map_err(|error| error.to_string())?;
        let chats = saved
            .iter()
            .filter(|chat| Some(chat.id) != current_chat.map(Into::into))
            .collect::<Vec<_>>();
        if chats.is_empty() {
            return Err("No conversations found to resume.".into());
        }
        let entries = chats
            .iter()
            .map(|chat| saved_conversation_entry(chat, &saved))
            .collect();
        let purpose = if as_new_chat {
            ConversationPickerPurpose::CopyAsNew
        } else {
            ConversationPickerPurpose::Resume
        };
        Ok((
            ConversationPicker::new(purpose, entries),
            Self::Chats {
                choices: chats.into_iter().map(|chat| chat.id).collect(),
                as_new_chat,
            },
        ))
    }

    pub(super) async fn commands(agent: &Agent, chat: Uuid) -> Result<(Menu, Self), String> {
        let mut commands = agent
            .commands(chat)
            .await
            .map_err(|error| error.to_string())?;
        commands.sort_by_key(|command| {
            (
                !matches!(
                    command.state,
                    CommandState::Running | CommandState::Background | CommandState::Cancelling
                ),
                command.command.clone(),
                command.operation_id,
            )
        });
        let snapshot = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        let mut user_shells = snapshot.user_shells;
        user_shells.sort_by_key(|command| {
            (
                !matches!(
                    command.state,
                    UserShellState::Queued
                        | UserShellState::Running
                        | UserShellState::Background
                        | UserShellState::Cancelling
                ),
                command.command.clone(),
                command.id,
            )
        });
        let options = user_shells
            .iter()
            .map(|command| {
                format!(
                    "User · {} · {}",
                    single_line(&command.command),
                    user_command_status(command)
                )
            })
            .chain(commands.iter().map(|command| {
                format!(
                    "Agent · {} · {}",
                    single_line(&command.command),
                    command_status(command)
                )
            }))
            .chain(std::iter::once("Cancel".into()))
            .collect();
        let context = if commands.is_empty() && user_shells.is_empty() {
            "No Shell tasks in this Chat yet."
        } else {
            "Choose a Shell task to read its output or manage its execution."
        };
        Ok((
            menu(
                "Shell tasks",
                vec![context.into(), PROCESS_LOCAL_TASKS_NOTE.into()],
                options,
            ),
            Self::Commands {
                chat,
                choices: user_shells
                    .into_iter()
                    .map(|command| CommandChoice::User(command.id))
                    .chain(
                        commands
                            .into_iter()
                            .map(|command| CommandChoice::Agent(command.operation_id)),
                    )
                    .collect(),
            },
        ))
    }

    pub(super) async fn plan(agent: &Agent, chat: Uuid) -> Result<(Menu, Self), String> {
        let status = agent
            .status(chat)
            .await
            .map_err(|error| error.to_string())?;
        let chat_loop = status.chat_loop.id;
        let plan = status
            .plans
            .iter()
            .find(|plan| plan.loop_id == chat_loop)
            .ok_or("This Chat has no plan yet. Ask the Agent to create one in Plan mode.")?;
        let mut context = vec![format!("Plan version: {}", plan.version)];
        if let Some(mode) = plan.approved_execution_mode {
            context.push(format!("Already approved with mode: {mode}"));
        }
        let markdown = Some(safe_text(&plan.markdown));
        if !plan.approval_pending {
            let mut menu = menu("Saved plan", context, vec!["Close".into()]);
            menu.markdown = markdown;
            return Ok((menu, Self::PlanView { chat }));
        }
        let request = status
            .permissions
            .iter()
            .find(|request| {
                request.name == tickloop_harness::tools::EXIT_PLAN_MODE_TOOL_NAME
                    && request.loop_id == chat_loop
            })
            .map(|request| request.id);
        let mut menu = menu(
            "Plan approval",
            context,
            PLAN_APPROVAL_CHOICES
                .iter()
                .map(|choice| choice.label().into())
                .collect(),
        );
        menu.markdown = markdown;
        menu.confirmation = true;
        Ok((
            menu,
            Self::Plan {
                chat,
                version: plan.version,
                request,
            },
        ))
    }

    #[cfg(test)]
    pub(super) async fn choose(
        self,
        agent: &Agent,
        console: &Console,
        index: Option<usize>,
    ) -> Result<WorkflowEffect, String> {
        self.choose_with_feedback(agent, console, index, None).await
    }

    pub(super) async fn choose_with_feedback(
        self,
        agent: &Agent,
        console: &Console,
        index: Option<usize>,
        feedback: Option<String>,
    ) -> Result<WorkflowEffect, String> {
        let Some(index) = index else {
            if let Self::Plan { chat, version, .. } = self {
                agent
                    .decline_plan(chat, version, None)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            return Ok(WorkflowEffect::default());
        };
        let mut effect = WorkflowEffect::default();
        match self {
            Self::PlanView { .. } => {}
            Self::Chats {
                choices,
                as_new_chat,
            } => {
                if index == choices.len() {
                    return Ok(effect);
                }
                let chosen = *choices.get(index).ok_or("Invalid Chat selection.")?;
                let snapshot = if as_new_chat {
                    agent.resume_as_new_chat(chosen.into()).await
                } else {
                    agent.resume_chat(chosen.into()).await
                }
                .map_err(|error| error.to_string())?;
                let chat = snapshot.id;
                super::ensure_snapshot_banner(console, &snapshot)?;
                console.reset_chat(chat.into())?;
                console.write(&format!(
                    "\nOpened Chat: {} · {}\n",
                    chat_title(&snapshot),
                    snapshot.id.public(),
                ))?;
                if let Some(origin) = &snapshot.branch_origin {
                    console.write(&format!(
                        "Branch of Chat: {}\n",
                        origin.source_chat_id.public()
                    ))?;
                }
                if as_new_chat {
                    console.write(&format!("Copied from Chat: {}\n", chosen.public()))?;
                }
                for warning in agent.transcript_load_warnings(chat.into()) {
                    super::write_warning(console, &warning)?;
                }
                super::show_settings_warnings(console, agent.user_settings_warnings(chat.into()))?;
                console.write("History only; selecting this Chat starts no work.\n")?;
                write_public_history(console, &snapshot)?;
                effect.selection = Some(chat.into());
                effect.restored = Some(snapshot);
            }
            Self::Commands { chat, choices } => {
                if index == choices.len() {
                    return Ok(effect);
                }
                let operation = *choices.get(index).ok_or("Invalid Shell task selection.")?;
                effect.next = Some(command_menu(agent, chat, operation, None).await?);
            }
            Self::Command {
                chat,
                operation,
                actions,
            } => {
                let action = actions.get(index).ok_or("Invalid Shell task action.")?;
                match action {
                    CommandAction::Output(offset) => {
                        let result = match operation {
                            CommandChoice::Agent(operation) => {
                                agent
                                    .read_command(
                                        chat,
                                        operation,
                                        *offset,
                                        COMMAND_OUTPUT_PAGE_BYTES,
                                    )
                                    .await
                            }
                            CommandChoice::User(operation) => {
                                agent
                                    .read_user_shell_output(
                                        chat,
                                        operation,
                                        *offset,
                                        COMMAND_OUTPUT_PAGE_BYTES,
                                    )
                                    .await
                            }
                        }
                        .map_err(|error| error.to_string())?;
                        let value: serde_json::Value = serde_json::from_str(&result.content)
                            .map_err(|_| "The command output response could not be read.")?;
                        let output = value["output"]
                            .as_str()
                            .ok_or("The command output response has no output field.")?;
                        let total = value["total_bytes"].as_u64().unwrap_or(0);
                        console.write(&format!(
                            "\n{} Shell output · offset {offset} · {total} total bytes\n",
                            operation.origin()
                        ))?;
                        if output.is_empty() {
                            console.write("No output at this offset.\n")?;
                        } else {
                            console.write(&safe_text(output))?;
                            if !output.ends_with('\n') {
                                console.write("\n")?;
                            }
                        }
                        if let Some(path) = result.output_ref {
                            console.write(&format!(
                                "Full output: {}\n",
                                safe_text(&path.to_string_lossy())
                            ))?;
                        }
                        let next_offset = value["next_offset"].as_u64();
                        if next_offset.is_some() {
                            console.write(
                                "More output is available; choose Read next output page.\n",
                            )?;
                        }
                        effect.next =
                            Some(command_menu(agent, chat, operation, next_offset).await?);
                    }
                    CommandAction::Background => {
                        match operation {
                            CommandChoice::Agent(operation) => {
                                let command = agent
                                    .background_command(chat, operation)
                                    .await
                                    .map_err(|error| error.to_string())?;
                                console.write(&format!(
                                    "\nAgent Shell task: {} · {}\n",
                                    single_line(&command.command),
                                    command_status(&command)
                                ))?;
                            }
                            CommandChoice::User(operation) => {
                                let command = agent
                                    .background_user_shell(chat, operation)
                                    .await
                                    .map_err(|error| error.to_string())?;
                                console.write(&format!(
                                    "\nUser Shell task: {} · {}\n",
                                    single_line(&command.command),
                                    user_command_status(&command)
                                ))?;
                            }
                        }
                        effect.next = Some(command_menu(agent, chat, operation, None).await?);
                    }
                    CommandAction::Stop => {
                        match operation {
                            CommandChoice::Agent(operation) => {
                                let command = agent
                                    .stop_command(chat, operation)
                                    .await
                                    .map_err(|error| error.to_string())?;
                                console.write(&format!(
                                    "\nAgent Shell task: {} · {}\n",
                                    single_line(&command.command),
                                    command_status(&command)
                                ))?;
                                if let Some(reason) = command.reason {
                                    console.write(&format!("Reason: {}\n", safe_text(&reason)))?;
                                }
                            }
                            CommandChoice::User(operation) => {
                                let command = agent
                                    .stop_user_shell(chat, operation)
                                    .await
                                    .map_err(|error| error.to_string())?;
                                console.write(&format!(
                                    "\nUser Shell task: {} · {}\n",
                                    single_line(&command.command),
                                    user_command_status(&command)
                                ))?;
                                if let Some(reason) = command.reason {
                                    console.write(&format!("Reason: {}\n", safe_text(&reason)))?;
                                }
                            }
                        }
                        effect.next = Some(command_menu(agent, chat, operation, None).await?);
                    }
                    CommandAction::Back => {
                        effect.next = Some(Self::commands(agent, chat).await?);
                    }
                }
            }
            Self::Plan { chat, version, .. } => {
                let choice = PLAN_APPROVAL_CHOICES
                    .get(index)
                    .ok_or("Invalid plan selection.")?;
                let Some(mode) = choice.mode() else {
                    effect.receipt = agent
                        .decline_plan(chat, version, feedback)
                        .await
                        .map_err(|error| error.to_string())?;
                    return Ok(effect);
                };
                let approval = agent
                    .approve_plan(chat, version, mode)
                    .await
                    .map_err(|error| error.to_string())?;
                if let Some(approval) = approval {
                    let receipt=agent.submit(chat,approval).await
                        .map_err(|error|format!("Plan version {version} was approved, but execution could not start: {error}"))?;
                    effect.receipt = Some(receipt);
                }
                console.write(&format!(
                    "\nPlan version {version} approved. Starting it with {mode} permissions.\n"
                ))?;
                effect.selection = Some(chat);
            }
        }
        Ok(effect)
    }
}

async fn command_menu(
    agent: &Agent,
    chat: Uuid,
    operation: CommandChoice,
    next_offset: Option<u64>,
) -> Result<(Menu, Workflow), String> {
    let (origin, command, cwd, status, output_bytes, output_ref, reason, can_background, can_stop) =
        match operation {
            CommandChoice::Agent(operation_id) => {
                let command = agent
                    .commands(chat)
                    .await
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .find(|command| command.operation_id == operation_id)
                    .ok_or("The selected Agent Shell task is no longer available.")?;
                let status = command_status(&command);
                let can_background = command.state == CommandState::Running;
                let can_stop = matches!(
                    command.state,
                    CommandState::Running | CommandState::Background
                );
                (
                    "Agent",
                    command.command,
                    command.cwd,
                    status,
                    command.output_bytes,
                    Some(command.output_ref),
                    command.reason,
                    can_background,
                    can_stop,
                )
            }
            CommandChoice::User(command_id) => {
                let command = agent
                    .snapshot(chat)
                    .await
                    .map_err(|error| error.to_string())?
                    .user_shells
                    .into_iter()
                    .find(|command| command.id == command_id)
                    .ok_or("The selected User Shell task is no longer available.")?;
                let status = user_command_status(&command);
                let can_background = command.state == UserShellState::Running;
                let can_stop = matches!(
                    command.state,
                    UserShellState::Running | UserShellState::Background
                );
                (
                    "User",
                    command.command,
                    command.cwd,
                    status,
                    command.output_bytes,
                    command.output_ref,
                    command.reason,
                    can_background,
                    can_stop,
                )
            }
        };
    let mut context = vec![
        format!("Origin: {origin}"),
        "Command:".into(),
        safe_text(&command),
        "Working directory:".into(),
        safe_text(&cwd.to_string_lossy()),
        format!("Status: {status}"),
        format!("Output bytes: {output_bytes}"),
        format!(
            "Full output: {}",
            output_ref
                .as_ref()
                .map(|path| safe_text(&path.to_string_lossy()))
                .unwrap_or_else(|| "Not available yet".into())
        ),
    ];
    if let Some(reason) = reason {
        context.push(format!("Reason: {}", safe_text(&reason)));
    }
    let mut options = Vec::new();
    let mut actions = Vec::new();
    if let Some(offset) = next_offset {
        options.push(format!("Read next output page · offset {offset}"));
        actions.push(CommandAction::Output(offset));
    }
    options.push("View output from the beginning".into());
    actions.push(CommandAction::Output(0));
    if can_background {
        options.push(format!("Move {origin} task to background"));
        actions.push(CommandAction::Background);
    }
    if can_stop {
        options.push(format!("Stop this {origin} task"));
        actions.push(CommandAction::Stop);
    }
    options.push("Back to Shell tasks".into());
    actions.push(CommandAction::Back);
    Ok((
        menu(&format!("{origin} Shell task"), context, options),
        Workflow::Command {
            chat,
            operation,
            actions,
        },
    ))
}

fn menu(title: &str, context: Vec<String>, options: Vec<String>) -> Menu {
    Menu {
        id: tickloop_ids::generate(),
        title: title.into(),
        context,
        markdown: None,
        options,
        selected: 0,
        confirmation: false,
    }
}

pub(super) fn chat_title(chat: &ChatSnapshot) -> String {
    if let Some(name) = chat.branch_name() {
        return single_line(name);
    }
    single_line(&tickloop_harness::protocol::chat_display_title(
        chat,
        &chat.user_shells,
    ))
}

/// Picker title: the Branch name or custom title, otherwise the first prompt.
fn saved_chat_title(chat: &SavedChat) -> String {
    chat.title
        .as_deref()
        .or(chat.first_prompt.as_deref())
        .map(compact_line)
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "New conversation".into())
}

/// Shows only metadata the saved record actually has; a Branch whose source is not among
/// this project's saved Chats omits the source rather than naming it by ID. The first prompt
/// behind a custom title stays searchable.
fn saved_conversation_entry(chat: &SavedChat, saved: &[SavedChat]) -> ConversationEntry {
    let mut metadata = vec![activity_age(chat.modified)];
    metadata.extend(chat.git_branch.clone());
    if let Some(source_title) = chat
        .branch_source
        .and_then(|source| saved.iter().find(|candidate| candidate.id == source))
        .map(saved_chat_title)
    {
        metadata.push(format!("Branch of {source_title}"));
    }
    let hidden_prompt = chat
        .title
        .as_ref()
        .and(chat.first_prompt.as_deref())
        .unwrap_or_default();
    ConversationEntry::new(&saved_chat_title(chat), &metadata, &[hidden_prompt])
}

pub(super) fn activity_age(modified: SystemTime) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    let seconds = SystemTime::now()
        .duration_since(modified)
        .map_or(0, |elapsed| elapsed.as_secs());
    match seconds {
        0..MINUTE => "just now".into(),
        MINUTE..HOUR => format!("{}m ago", seconds / MINUTE),
        HOUR..DAY => format!("{}h ago", seconds / HOUR),
        _ => format!("{}d ago", seconds / DAY),
    }
}

fn single_line(text: &str) -> String {
    safe_text(text).replace('\n', " ↵ ").replace('\t', " ")
}

fn command_status(command: &CommandSnapshot) -> String {
    let mut status = format!("{:?}", command.state);
    if let Some(code) = command.exit_code {
        status.push_str(&format!(" · exit {code}"));
    }
    if let Some(signal) = command.signal {
        status.push_str(&format!(" · signal {signal}"));
    }
    status
}

fn user_command_status(command: &UserShellRecord) -> String {
    let mut status = format!("{:?}", command.state);
    if let Some(code) = command.exit_code {
        status.push_str(&format!(" · exit {code}"));
    }
    if let Some(signal) = command.signal {
        status.push_str(&format!(" · signal {signal}"));
    }
    status
}

/// Text the current Run has streamed but not yet written into the conversation, derived only
/// from runtime events and not saved with the Chat.
pub(super) fn pending_reply(snapshot: &ChatSnapshot) -> String {
    let visible_events = snapshot.events.iter().collect::<Vec<_>>();
    let Some(streaming_run) = visible_events
        .last()
        .filter(|event| matches!(event.kind, EventKind::Text(_)))
        .map(|event| event.run_id)
    else {
        return String::new();
    };
    let is_running = snapshot.chat_loop.run_id == Some(streaming_run)
        && snapshot.chat_loop.state == LoopState::Running;
    let last_message_index = snapshot.messages.len().checked_sub(1);
    let is_committed = last_message_index.is_some_and(|index| {
        snapshot.messages[index].role == Role::Assistant
            && snapshot.message_runs.get(&index) == Some(&streaming_run)
    });
    if !is_running || is_committed {
        return String::new();
    }
    visible_events
        .iter()
        .rev()
        .take_while(|event| event.run_id == streaming_run)
        .map_while(|event| match &event.kind {
            EventKind::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

struct PublicHistoryBuilder<'a> {
    tick_labels: super::tick_labels::TickLabels,
    chat: &'a ChatSnapshot,
    pieces: Vec<ConversationPiece>,
    image_links: ImageLinks,
    colors: &'a AgentColors,
    tool_states: HashMap<(Uuid, String), super::scrollback::ToolCallState>,
    /// The result of each Subagent call, which names the child its row shows.
    subagent_launches: HashMap<ToolCallKey, serde_json::Value>,
    /// Children whose completion block already stands at their report or completion notice.
    shown_completions: HashSet<Uuid>,
    /// Set while the latest piece is a declined call's result, which already stands for the
    /// interruption that follows it.
    after_rejection: bool,
}

/// A call as its Chat, Run and id name it.
type ToolCallKey = (Uuid, Option<Uuid>, String);

impl PublicHistoryBuilder<'_> {
    fn push_str(&mut self, text: &str) {
        self.push_runs(&[(text.to_owned(), None)]);
    }

    fn push_role(&mut self, text: String, role: super::style::Role) {
        self.push_runs(&[(text, Some(role))]);
    }

    /// Runs written as one entry; a first run that begins with a newline starts a block.
    fn push_runs(&mut self, runs: &[(String, Option<super::style::Role>)]) {
        for piece in ConversationPiece::block_runs(runs) {
            match piece {
                ConversationPiece::Styled(text, role) => self.push(text, role),
                piece => self.pieces.push(piece),
            }
        }
    }

    fn push_response(&mut self, runs: Vec<(String, Option<super::style::Role>)>) {
        if !runs.is_empty() {
            self.pieces.push(ConversationPiece::Response(runs));
        }
    }

    /// An Agent block: its heading and the body indented under it.
    fn push_agent_block(
        &mut self,
        (heading, body): (presentation::RoleRuns, presentation::RoleRuns),
    ) {
        self.push_runs(&heading);
        if !body.is_empty() {
            self.pieces.push(ConversationPiece::Body(body));
        }
    }

    /// Prompt text of `chat`, with each cached `[Image #N]` linked to its image.
    fn push_prompt(
        &mut self,
        chat: Uuid,
        text: &str,
        display: DisplayProfile,
        is_user_message: bool,
    ) {
        let links = self.image_links.in_text(chat, text);
        let (fill, spans) = super::console::input_spans(text, display, &links);
        let text = spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>();
        self.pieces.push(ConversationPiece::Input {
            text: format!("\n{text}\n"),
            spans,
            fill,
            is_user_message,
        });
    }

    fn push(&mut self, text: String, role: Option<super::style::Role>) {
        if text.is_empty() {
            return;
        }
        if let Some(ConversationPiece::Styled(last_text, last_role)) = self.pieces.last_mut()
            && *last_role == role
        {
            last_text.push_str(&text);
        } else {
            self.pieces.push(ConversationPiece::Styled(text, role));
        }
    }

    fn push_reply(&mut self, text: &str) {
        if !text.is_empty() {
            self.pieces.push(ConversationPiece::Markdown {
                text: text.to_owned(),
                starts_message: true,
            });
        }
    }
}

fn public_history_pieces(
    chat: &ChatSnapshot,
    display: DisplayProfile,
    image_links: ImageLinks,
    colors: &AgentColors,
) -> Vec<ConversationPiece> {
    let Ok(projection) = chat.full_public_conversation_items() else {
        return vec![ConversationPiece::Styled(
            "\nConversation view unavailable. ctrl+o opens the detailed transcript.\n".into(),
            None,
        )];
    };
    let mut history = PublicHistoryBuilder {
        tick_labels: super::tick_labels::TickLabels::from_snapshot(chat),
        chat,
        pieces: Vec::new(),
        image_links,
        colors,
        tool_states: presentation::tool_call_states(chat),
        subagent_launches: subagent_launches(&projection),
        shown_completions: HashSet::new(),
        after_rejection: false,
    };
    let mut tool_calls = HashMap::new();
    let mut answers_tick_delivery = false;
    let mut goal_history = chat.goal_history.iter().peekable();
    while let Some(entry) = goal_history.next_if(|entry| entry.after_message_id.is_none()) {
        history.pieces.extend(super::goal_view::notice_pieces(
            &entry.notice,
            &entry.objective,
        ));
    }
    for item in projection {
        match item {
            BranchPublicItem::Summary { .. } => {
                history.push_runs(&directed_summary::history_marker(display));
                answers_tick_delivery = false;
            }
            BranchPublicItem::Message {
                source,
                message,
                contexts,
                has_thinking,
            } => {
                if has_thinking {
                    history.pieces.push(presentation::thinking_label());
                }
                if let Some(origin) = source
                    .message_id
                    .and_then(|id| chat.human_family_origins.get(&id))
                {
                    history.push_role(
                        format!(
                            "\nUser message · from @{} View\n",
                            safe_text(&origin.source_handle)
                        ),
                        super::style::Role::MutedText,
                    );
                }
                let folded = answers_tick_delivery && is_folded_tick_reply(&message);
                answers_tick_delivery = is_tick_delivery(&message);
                append_public_message(
                    &mut history,
                    &mut tool_calls,
                    &source,
                    &contexts,
                    &message,
                    display,
                    folded,
                );
                while let Some(entry) =
                    goal_history.next_if(|entry| entry.after_message_id == source.message_id)
                {
                    history.pieces.extend(super::goal_view::notice_pieces(
                        &entry.notice,
                        &entry.objective,
                    ));
                }
            }
        }
    }
    for entry in goal_history {
        history.pieces.extend(super::goal_view::notice_pieces(
            &entry.notice,
            &entry.objective,
        ));
    }
    let pending = pending_reply(chat);
    if !pending.is_empty() {
        history.push_reply(&safe_text(&pending));
    }
    for record in chat.family_inputs.values() {
        history.push_role(
            presentation::family_input_text(record, presentation::OutputDetail::Preview),
            super::style::Role::MutedText,
        );
    }
    // A background child whose report or completion notice has not reached the conversation.
    let shown_completions = std::mem::take(&mut history.shown_completions);
    for completion in chat.family_completions.iter().filter(|completion| {
        completion.notification.is_background()
            && !shown_completions.contains(&completion.child_run_id.uuid())
    }) {
        history.push_agent_block(agent_messaging::completion_runs(
            completion,
            chat,
            colors,
            presentation::OutputDetail::Preview,
            display,
        ));
    }
    history.pieces
}

/// The result of every Subagent call in `projection`, by the call it answers.
fn subagent_launches(projection: &[BranchPublicItem]) -> HashMap<ToolCallKey, serde_json::Value> {
    projection
        .iter()
        .filter_map(|item| match item {
            BranchPublicItem::Message {
                source, message, ..
            } => Some((source, message)),
            BranchPublicItem::Summary { .. } => None,
        })
        .flat_map(|(source, message)| {
            message.content.iter().filter_map(|content| match content {
                Content::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    let text = content.text();
                    agent_messaging::launched_child(&text)?;
                    Some((
                        (
                            source.chat_id.uuid(),
                            source.run_id.map(|run| run.uuid()),
                            tool_use_id.clone(),
                        ),
                        serde_json::from_str(&text).ok()?,
                    ))
                }
                _ => None,
            })
        })
        .collect()
}

/// The history as plain text, with each reply's Markdown source behind the
/// assistant glyph.
#[cfg(test)]
pub(super) fn public_history(chat: &ChatSnapshot, display: DisplayProfile) -> String {
    let assistant_lead = format!("{} ", display.glyph(super::display::Glyph::Assistant));
    public_history_pieces(
        chat,
        display,
        ImageLinks::default(),
        &AgentColors::default(),
    )
    .iter()
    .map(|piece| match piece {
        ConversationPiece::Markdown { text, .. } => format!("{assistant_lead}{text}"),
        piece => piece.text().into_owned(),
    })
    .collect()
}

pub(super) fn write_public_history(console: &Console, chat: &ChatSnapshot) -> Result<(), String> {
    if let Some(delegation) = &chat.delegation {
        let task = single_line(&delegation.task);
        if console.is_interactive() {
            console.write(&format!("Delegated Agent · {task}\n"))?;
        } else {
            console.write(&format!(
                "Agent family · root {} / child {}\nTask: {}\n",
                delegation.parent_chat_id.public(),
                chat.id.public(),
                task
            ))?;
        }
    }
    let pieces = public_history_pieces(
        chat,
        console.display_profile()?,
        console.image_links()?,
        &console.agent_colors()?,
    );
    console.write_pieces(&pieces)?;
    console.mark_input_echoes(
        chat.id.into(),
        chat.message_runs
            .values()
            .copied()
            .filter(|input| {
                super::consumed_input_text(chat, (*input).into()).is_some()
                    || matches!(chat.trigger_origins.get(input), Some(tickloop_harness::protocol::TriggerIdentity::GoalContinuation { .. }))
            })
            .map(|input| input.uuid())
            .chain(
                chat.messages
                    .iter()
                    .enumerate()
                    .filter(|(_, message)| is_tick_delivery(message) || (message.role == Role::User && message.content.iter().any(|content| matches!(content, Content::Text { text } if matches!(LocalMessage::parse(text), Some(LocalMessage::ArtifactRequest { .. }))))))
                    .filter_map(|(index, _)| chat.message_runs.get(&index).map(|run| run.uuid())),
            )
            .chain(agent_messaging::shown_agent_inputs(chat)),
    )?;
    if console.is_interactive()
        && let Some(event) = chat.conversation_events().into_iter().rev().find(|event| {
            event.chat_id == chat.id
                && matches!(event.kind, EventKind::Started | EventKind::Completed { .. })
        })
        && matches!(event.kind, EventKind::Completed { .. })
    {
        super::write_completion_notice(console, None)?;
    }
    console.replay_execution_losses(chat.id.into())?;
    Ok(())
}

/// A Tick Update delivery: the Chat shows its lit-tick lines, never the payload the model reads.
fn is_tick_delivery(message: &tickloop_harness::protocol::Message) -> bool {
    message.role == Role::User
        && message.content.iter().any(|content| match content {
            Content::Text { text } => {
                matches!(
                    LocalMessage::parse(text),
                    Some(LocalMessage::TickDelivery { .. })
                )
            }
            _ => false,
        })
}

/// A quiet answer to a delivery: one short piece of text and no tool call, so it folds into one
/// muted line instead of a full reply block.
fn is_folded_tick_reply(message: &tickloop_harness::protocol::Message) -> bool {
    message.role == Role::Assistant
        && message.content.iter().all(|content| match content {
            Content::Text { text } => text.chars().count() <= FOLDED_TICK_REPLY_CHARACTERS,
            _ => false,
        })
}

/// An Agent message in saved history: a background child's report or completion notice as the
/// child's completion block, any other message as its line. Nothing when it shows elsewhere: a
/// foreground child's report under its call, or a report whose run has not ended yet.
fn push_agent_input(
    history: &mut PublicHistoryBuilder,
    source: &BranchPublicSource,
    local: &LocalMessage,
    display: DisplayProfile,
) {
    let preview = presentation::OutputDetail::Preview;
    match agent_messaging::agent_input(history.chat, source.message_id, local) {
        AgentInput::Result(completion) => {
            if !completion.notification.is_background() {
                return;
            }
            history
                .shown_completions
                .insert(completion.child_run_id.into());
            let block = agent_messaging::completion_runs(
                completion,
                history.chat,
                history.colors,
                preview,
                display,
            );
            history.push_agent_block(block);
        }
        AgentInput::PendingResult => {}
        AgentInput::Message => match AgentMessage::of(local) {
            Some(message) => {
                history.push_agent_block(agent_messaging::message_runs(&message, preview, display));
            }
            None => {
                let (heading, response) = presentation::local_message_runs(local, preview, display);
                history.push_runs(&heading);
                history.push_response(response);
            }
        },
    }
}

fn append_public_message(
    history: &mut PublicHistoryBuilder,
    tool_calls: &mut HashMap<(Uuid, Option<Uuid>, String), (String, serde_json::Value)>,
    source: &BranchPublicSource,
    contexts: &[InputContext],
    message: &tickloop_harness::protocol::Message,
    display: DisplayProfile,
    folded: bool,
) {
    let (source_chat_id, run_id) = (source.chat_id, source.run_id);
    let trigger = (source_chat_id == history.chat.id && message.role == Role::User)
        .then(|| run_id.and_then(|run| history.chat.trigger_origins.get(&run)))
        .flatten();
    if matches!(
        trigger,
        Some(tickloop_harness::protocol::TriggerIdentity::GoalContinuation { .. })
    ) {
        history.push_runs(&super::goal_view::continuation_runs());
        return;
    }
    append_input_contexts(history, contexts);
    for content in &message.content {
        let follows_rejection = std::mem::take(&mut history.after_rejection);
        match content {
            Content::Text { text }
                if conversation::is_interruption_placeholder(message.role, text) => {}
            Content::Text { text }
                if message.role == Role::User
                    && let Some(local_message) =
                        presentation::typed_local_message(text, source.is_agent_message) =>
            {
                let local_message =
                    presentation::name_local_ticks(local_message, &history.tick_labels);
                if let LocalMessage::BashInput { command } = &local_message {
                    history.push_prompt(
                        source_chat_id.into(),
                        &format!("!{}", safe_text(command)),
                        display,
                        false,
                    );
                    continue;
                }
                if matches!(local_message, LocalMessage::Interrupted { .. }) && follows_rejection {
                    continue;
                }
                if source.is_agent_message {
                    push_agent_input(history, source, &local_message, display);
                    continue;
                }
                let (heading, response) = presentation::local_message_runs(
                    &local_message,
                    presentation::OutputDetail::Preview,
                    display,
                );
                history.push_runs(&heading);
                history.push_response(response);
            }
            Content::Text { text } => match message.role {
                Role::User => {
                    let text = trigger.map_or_else(
                        || std::borrow::Cow::Borrowed(text.as_str()),
                        |trigger| trigger.display_input(text),
                    );
                    history.push_prompt(
                        source_chat_id.into(),
                        &safe_text(&text),
                        display,
                        !source.is_agent_message,
                    );
                    if source_chat_id == history.chat.id
                        && let Some(skill) =
                            run_id.and_then(|run| history.chat.user_skill_invocations.get(&run))
                    {
                        history.push_runs(&presentation::user_skill_runs(skill, display));
                    }
                }
                Role::Assistant if folded => {
                    history.push_role(
                        format!("  {}\n", safe_text(text.trim()).replace('\n', " ")),
                        super::style::Role::MutedText,
                    );
                }
                Role::Assistant => history.push_reply(&safe_text(text)),
                Role::System => {}
            },
            Content::ToolUse { id, name, input } if message.role == Role::Assistant => {
                tool_calls.insert(
                    (source_chat_id.into(), run_id.map(Into::into), id.clone()),
                    (name.clone(), input.clone()),
                );
                if presentation::has_no_call_row(name) {
                    continue;
                }
                if let Some(run) = run_id {
                    let state = history
                        .tool_states
                        .get(&(run.into(), id.clone()))
                        .copied()
                        .unwrap_or(super::scrollback::ToolCallState::Finished);
                    let spans = if name == SUBAGENT_TOOL_NAME {
                        agent_messaging::subagent_call_runs(
                            input,
                            history.subagent_launches.get(&(
                                source_chat_id.into(),
                                run_id.map(Into::into),
                                id.clone(),
                            )),
                            history.colors,
                            display,
                        )
                    } else {
                        presentation::tool_call_roles(
                            name,
                            input,
                            super::shown_folder(history.chat.folder.as_deref()),
                            display,
                            &history.tick_labels,
                        )
                    };
                    history.pieces.push(ConversationPiece::ToolCall {
                        run_id: run.into(),
                        text: tool_call_text(&spans, state),
                        call_key: presentation::tool_call_key(run.into(), id),
                        spans,
                        state,
                        collapse: (!history.chat.approval_records.iter().any(|record| {
                            record.run_id == run
                                && record.call_id == *id
                                && record.path != tickloop_harness::protocol::ApprovalPath::Direct
                        }))
                        .then(|| {
                            history
                                .tick_labels
                                .native_tool_instance(name, input)
                                .is_none()
                                .then(|| presentation::collapsed_call(name, input))
                                .flatten()
                        })
                        .flatten(),
                        parent_key: None,
                    });
                    if presentation::makes_nested_calls(name) {
                        append_nested_calls(history, run.into(), id, display);
                    }
                    let approvals =
                        super::approval_status::ToolApprovals::from_snapshot(history.chat);
                    if let Some(approval) = approvals.get(run.into(), id)
                        && approval.pending_label().is_some()
                    {
                        history.pieces.push(ConversationPiece::ToolApproval {
                            call_key: presentation::tool_call_key(run.into(), id),
                            runs: approval.decorate(Vec::new(), display.glyph(Glyph::ToolResult)),
                        });
                    }
                } else {
                    history.push_role(
                        presentation::archived_tool_call(
                            name,
                            input,
                            super::shown_folder(history.chat.folder.as_deref()),
                            display,
                            &history.tick_labels,
                        ),
                        super::style::Role::MutedText,
                    );
                }
            }
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if message.role == Role::User => {
                let call = tool_calls.remove(&(
                    source_chat_id.into(),
                    run_id.map(Into::into),
                    tool_use_id.clone(),
                ));
                let (name, input) = call
                    .as_ref()
                    .map_or(("tool", None), |(name, input)| (name.as_str(), Some(input)));
                let result = ToolResult {
                    content: content.text().into_owned(),
                    is_error: *is_error,
                    output_ref: None,
                };
                if name == tickloop_harness::tools::ASK_USER_QUESTION_TOOL_NAME {
                    if let Some((heading, response)) = presentation::question_result_runs(
                        input,
                        &result,
                        display.glyph(Glyph::ToolCall),
                        display.glyph(Glyph::ToolResult),
                    ) {
                        history.push_runs(&heading);
                        history.push_response(response);
                    }
                    history.after_rejection = presentation::is_rejection(&result);
                    continue;
                }
                if presentation::has_no_call_row(name) {
                    // TodoWrite shows its call only when it fails.
                    if !*is_error {
                        continue;
                    }
                    if let (Some(run), Some(input)) = (run_id, input) {
                        let spans = presentation::tool_call_roles(
                            name,
                            input,
                            super::shown_folder(history.chat.folder.as_deref()),
                            display,
                            &history.tick_labels,
                        );
                        history.pieces.push(ConversationPiece::ToolCall {
                            run_id: run.into(),
                            text: spans.iter().map(|(text, _)| text.as_str()).collect(),
                            call_key: presentation::tool_call_key(run.into(), tool_use_id),
                            spans,
                            state: super::scrollback::ToolCallState::Failed,
                            collapse: None,
                            parent_key: None,
                        });
                    }
                }
                let facts = presentation::CallFacts {
                    review: run_id
                        .filter(|_| name == tickloop_harness::tools::EDIT_TOOL_NAME && !*is_error)
                        .and_then(|run| {
                            presentation::file_change_review(history.chat, run.into(), tool_use_id)
                        }),
                    report: history
                        .subagent_launches
                        .get(&(
                            source_chat_id.into(),
                            run_id.map(Into::into),
                            tool_use_id.clone(),
                        ))
                        .filter(|_| name == SUBAGENT_TOOL_NAME)
                        .and_then(|launch| agent_messaging::subagent_report(history.chat, launch)),
                };
                let mut runs = presentation::archived_tool_result(
                    name,
                    input,
                    &result,
                    super::shown_folder(history.chat.folder.as_deref()),
                    display,
                    &facts,
                );
                if let Some(run) = run_id {
                    if presentation::makes_nested_calls(name)
                        && let Some(summary) = presentation::settled_batch_summary(
                            &result.content,
                            &super::approval_status::ToolApprovals::from_snapshot(history.chat),
                            run.into(),
                        )
                        .or_else(|| {
                            presentation::snapshot_batch_summary(
                                history.chat,
                                run.into(),
                                tool_use_id,
                                input,
                            )
                        })
                    {
                        runs = super::approval_status::result_runs(
                            &summary,
                            display.glyph(Glyph::ToolResult),
                        );
                    }
                    let approvals =
                        super::approval_status::ToolApprovals::from_snapshot(history.chat);
                    if let Some(approval) = approvals.get(run.into(), tool_use_id) {
                        runs = approval.decorate(runs, display.glyph(Glyph::ToolResult));
                    }
                }
                match run_id {
                    Some(run) => history.pieces.push(ConversationPiece::ToolResult {
                        call_key: presentation::tool_call_key(run.into(), tool_use_id),
                        clipped_rows: super::approval_status::ToolApprovals::from_snapshot(
                            history.chat,
                        )
                        .get(run.into(), tool_use_id)
                        .map_or_else(Vec::new, |approval| approval.clipped_rows(&runs)),
                        runs,
                        clip_summary: presentation::goal_completion_rejection_reason(
                            name,
                            &result.content,
                            result.is_error,
                        )
                        .is_some(),
                    }),
                    None => history.push_response(runs),
                }
                history.after_rejection = presentation::is_rejection(&result);
            }
            Content::ToolUse { .. }
            | Content::ToolResult { .. }
            | Content::Thinking { .. }
            | Content::ProtocolData { .. }
            | Content::Image { .. } => {}
        }
    }
}

/// A call row's plain text: its spans with the state's suffix before the final newline.
fn tool_call_text(
    spans: &[(String, Option<super::style::Role>)],
    state: super::scrollback::ToolCallState,
) -> String {
    let mut text = spans
        .iter()
        .map(|(text, _)| text.as_str())
        .collect::<String>();
    text.truncate(text.len().saturating_sub(1));
    text.push_str(state.suffix());
    text.push('\n');
    text
}

/// The calls a batch made, each with its result, under the row of the call that made them.
fn append_nested_calls(
    history: &mut PublicHistoryBuilder,
    run: Uuid,
    parent: &str,
    display: DisplayProfile,
) {
    let folder = super::shown_folder(history.chat.folder.as_deref());
    let parent_key = presentation::tool_call_key(run, parent);
    let approvals = super::approval_status::ToolApprovals::from_snapshot(history.chat);
    for operation in presentation::nested_operations(history.chat, run, parent) {
        let call_key = presentation::tool_call_key(run, &operation.call_id);
        let spans = presentation::tool_call_roles(
            &operation.name,
            &operation.input,
            folder,
            display,
            &history.tick_labels,
        );
        let state = match &operation.result {
            Some(result) => {
                presentation::tool_result_state(&operation.name, &result.content, result.is_error)
            }
            None => history
                .tool_states
                .get(&(run, operation.call_id.clone()))
                .copied()
                .unwrap_or(super::scrollback::ToolCallState::Finished),
        };
        history.pieces.push(ConversationPiece::ToolCall {
            run_id: run,
            text: tool_call_text(&spans, state),
            call_key: call_key.clone(),
            spans,
            state,
            collapse: None,
            parent_key: Some(parent_key.clone()),
        });
        let approval = approvals.get(run, &operation.call_id);
        if let Some(result) = &operation.result {
            let mut runs = presentation::archived_tool_result(
                &operation.name,
                Some(&operation.input),
                result,
                folder,
                display,
                &presentation::CallFacts::default(),
            );
            if let Some(approval) = approval {
                runs = approval.decorate(runs, display.glyph(Glyph::ToolResult));
            }
            history.pieces.push(ConversationPiece::ToolResult {
                call_key,
                clipped_rows: approval
                    .map_or_else(Vec::new, |approval| approval.clipped_rows(&runs)),
                runs,
                clip_summary: false,
            });
        } else if let Some(approval) = approval {
            history.pieces.push(ConversationPiece::ToolResult {
                call_key,
                runs: approval.decorate(Vec::new(), display.glyph(Glyph::ToolResult)),
                clipped_rows: vec![0, 1],
                clip_summary: false,
            });
        }
    }
    let input = history
        .chat
        .operations
        .iter()
        .find(|operation| operation.run_id.uuid() == run && operation.call_id == parent)
        .map(|operation| &operation.input);
    if presentation::nested_operations(history.chat, run, parent)
        .any(|operation| operation.result.is_none())
        && let Some(summary) =
            presentation::snapshot_batch_summary(history.chat, run, parent, input)
    {
        history.pieces.push(ConversationPiece::ToolResult {
            call_key: parent_key,
            runs: super::approval_status::result_runs(&summary, display.glyph(Glyph::ToolResult)),
            clipped_rows: Vec::new(),
            clip_summary: false,
        });
    }
}

fn append_input_contexts(history: &mut PublicHistoryBuilder, contexts: &[InputContext]) {
    for context in contexts {
        let (kind, label, truncated) = match context {
            InputContext::File(text) => ("file", text.path.display().to_string(), text.truncated),
            InputContext::Directory(text) => {
                ("directory", text.path.display().to_string(), text.truncated)
            }
            InputContext::Image(image) => ("image", image.origin.label(), false),
        };
        let truncated = if truncated { " · truncated" } else { "" };
        history.push_str(&format!(
            "\nAttached {kind}: {}{truncated}\n",
            safe_text(&label)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
    use serde_json::json;
    use std::sync::Arc;
    use tickloop_harness::protocol::{BranchRequest, CompactionMarker, Event, SummaryDirection};
    use tickloop_harness::protocol::{Decision, PermissionRule};
    use tickloop_harness::protocol::{InputModality, Message, ModelConfig, Protocol};

    async fn completed(
        events: &mut tokio::sync::broadcast::Receiver<tickloop_harness::protocol::Event>,
    ) {
        loop {
            match tickloop_harness::test_support::next_event(events)
                .await
                .kind
            {
                EventKind::Completed { .. } => return,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn retained_history_keeps_completion_notices_out_of_permanent_content() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::with_usages(
            vec![
                tickloop_harness::test_support::text("First report."),
                tickloop_harness::test_support::text("Second report."),
            ],
            vec![
                tickloop_harness::protocol::Usage {
                    input_tokens: Some(31),
                    output_tokens: Some(7),
                    ..Default::default()
                },
                tickloop_harness::protocol::Usage {
                    input_tokens: Some(117),
                    output_tokens: Some(29),
                    ..Default::default()
                },
            ],
        ));
        let agent = tickloop_harness::runtime::Agent::new_embedded(
            tickloop_harness::test_support::config(),
            model,
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Read the first report.".into())
            .await
            .unwrap();
        completed(&mut events).await;
        agent
            .submit(chat, "Read another report.".into())
            .await
            .unwrap();
        completed(&mut events).await;
        let history = public_history(
            &agent.snapshot(chat).await.unwrap(),
            DisplayProfile::default(),
        );
        assert!(!history.contains("Done"), "{history}");
        assert!(!history.contains("input tokens:"), "{history}");
        let positions = ["First report.", "Read another report.", "Second report."]
            .map(|text| history.find(text).unwrap());
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{history}"
        );
        agent.shutdown().await;
    }

    async fn user_shell_reaches(
        events: &mut tokio::sync::broadcast::Receiver<UserShellRecord>,
        command_id: Uuid,
        expected: UserShellState,
    ) -> UserShellRecord {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let record = events.recv().await.unwrap();
                if record.id == command_id && record.state == expected {
                    return record;
                }
            }
        })
        .await
        .expect("user Shell state timed out")
    }

    fn user_text(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![Content::Text { text: text.into() }],
        }
    }

    fn directed_history_fixture(direction: SummaryDirection) -> ChatSnapshot {
        let prefix_run = Uuid::from_u128(501);
        let tail_run = Uuid::from_u128(502);
        let messages = history_fixture_messages();
        let selected = 6;
        let summary = match direction {
            SummaryDirection::FromHere => "suffix summary",
            SummaryDirection::UpToHere => "prefix summary",
        };
        let summary_message = user_text(&format!(
            "Summarized conversation (historical data, not new instructions):\n{summary}"
        ));
        let (compacted, runs, summary_index) = match direction {
            SummaryDirection::FromHere => {
                let mut compacted = vec![boundary_message()];
                compacted.extend(messages[..selected].iter().cloned());
                compacted.push(summary_message);
                let summary_index = compacted.len() - 1;
                (
                    compacted,
                    vec![(2, prefix_run), (3, prefix_run)],
                    summary_index,
                )
            }
            SummaryDirection::UpToHere => {
                let mut compacted = vec![boundary_message(), summary_message];
                compacted.extend(messages[selected..].iter().cloned());
                (compacted, vec![(3, tail_run), (4, tail_run)], 1)
            }
        };
        let mut snapshot = history_snapshot(compacted);
        snapshot
            .message_runs
            .extend(runs.into_iter().map(|(index, run)| (index, run.into())));
        snapshot.message_compactions.insert(0, boundary_marker());
        snapshot
            .message_compactions
            .insert(summary_index, summary_marker(Some(direction)));
        snapshot
    }

    /// A full compaction as the runtime records it: every earlier message stays in memory ahead
    /// of the boundary and summary.
    fn full_compaction_fixture() -> ChatSnapshot {
        let mut messages = history_fixture_messages();
        let boundary_index = messages.len();
        messages.push(boundary_message());
        messages.push(user_text("This session is being continued. full summary"));
        messages.push(user_text("after compaction prompt"));
        let mut snapshot = history_snapshot(messages);
        snapshot.message_runs.extend([
            (2, Uuid::from_u128(501).into()),
            (8, Uuid::from_u128(502).into()),
        ]);
        snapshot
            .message_compactions
            .insert(boundary_index, boundary_marker());
        snapshot
            .message_compactions
            .insert(boundary_index + 1, summary_marker(None));
        snapshot
    }

    fn boundary_message() -> Message {
        Message {
            role: Role::System,
            content: vec![Content::Text {
                text: "Conversation compacted".into(),
            }],
        }
    }

    fn history_fixture_messages() -> Vec<Message> {
        vec![
            Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "prefix prompt".into(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: "prefix-call".into(),
                    name: "Read".into(),
                    input: json!({"file_path": "prefix.txt"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: "prefix-call".into(),
                    content: json!({"text": "prefix tool output"}).to_string().into(),
                    is_error: false,
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![Content::Text {
                    text: "prefix answer".into(),
                }],
            },
            user_text("<bash-input>prefix-shell</bash-input>"),
            user_text("<bash-stdout>prefix shell output</bash-stdout><bash-stderr></bash-stderr>"),
            Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "selected prompt".into(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: "tail-call".into(),
                    name: "Bash".into(),
                    input: json!({"command": "printf tail"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: "tail-call".into(),
                    content: json!({
                        "state": "Completed",
                        "output": "tail tool output"
                    })
                    .to_string()
                    .into(),
                    is_error: false,
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![Content::Text {
                    text: "tail answer".into(),
                }],
            },
            user_text("<bash-input>tail-shell</bash-input>"),
            user_text("<bash-stdout>tail shell output</bash-stdout><bash-stderr></bash-stderr>"),
        ]
    }

    fn history_snapshot(messages: Vec<Message>) -> ChatSnapshot {
        serde_json::from_value(json!({
            "id": Uuid::from_u128(500),
            "folder": "/workspace",
            "model": ModelConfig {
                provider: tickloop_harness::protocol::ModelProvider::Api,
                auth_source: tickloop_harness::protocol::ModelAuthSource::ApiKeyEnv,
                name: "fixture".into(),
                protocol: Protocol::Messages,
                base_url: "https://example.invalid".into(),
                model: "fixture".into(),
                api_key_env: "UNUSED_HISTORY_FIXTURE_KEY".into(),
                max_output_tokens: 128,
                provider_id: String::new(),
                provider_name: String::new(),
                model_id: String::new(),
                connection: None,
                card: None,
                input_modalities: vec![InputModality::Text],
            },
            "messages": messages,
            "chat_loop": {"id":"00000000-0000-0000-0000-000000000000","state":"Idle"},
            "last_sequence": 20,
            "last_queue_sequence": 1,
        }))
        .unwrap()
    }

    fn boundary_marker() -> CompactionMarker {
        serde_json::from_value(json!({"Boundary": {
            "trigger": "manual",
            "pre_tokens": 0,
            "user_context": null,
            "messages_summarized": null,
            "preserved_by_later_summary": false,
        }}))
        .unwrap()
    }

    fn summary_marker(direction: Option<SummaryDirection>) -> CompactionMarker {
        CompactionMarker::Summary {
            summarize_metadata: direction.map(|direction| {
                tickloop_harness::protocol::SummarizeMetadata {
                    messages_summarized: 6,
                    user_context: None,
                    direction,
                }
            }),
        }
    }

    fn tick_delivery_history(reply: &str) -> ChatSnapshot {
        delivery_history(
            reply,
            json!({
                "items": [{
                    "subscription": Uuid::from_u128(700),
                    "instance": "calendar:fixture-run-3",
                    "title": "Quote board",
                    "updates": 5,
                    "seq": "8→12",
                    "between": "2026-09-21T03:11:00Z→2026-09-21T03:15:00Z",
                    "gap": false,
                    "data": {"last_done": 191.2},
                }],
            }),
        )
    }

    fn delivery_history(reply: &str, payload: serde_json::Value) -> ChatSnapshot {
        serde_json::from_value(json!({
            "id": Uuid::from_u128(701),
            "folder": "/workspace",
            "messages": [
                user_text(&tickloop_harness::protocol::local_messages::tick_delivery(&payload)),
                Message {
                    role: Role::Assistant,
                    content: vec![Content::Text { text: reply.into() }],
                },
            ],
            "chat_loop": {"id":"00000000-0000-0000-0000-000000000000","state":"Idle"},
        }))
        .unwrap()
    }

    #[test]
    fn a_tick_delivery_shows_one_line_and_keeps_its_payload_out_of_the_chat() {
        let history = public_history(&tick_delivery_history("quiet"), DisplayProfile::default());

        assert!(
            history.contains("◉ TickUpdate: Quote board  #8→12 · 5 merged\n"),
            "{history}"
        );
        assert!(!history.contains("last_done"), "{history}");
        assert!(!history.contains("watch 700.HK"), "{history}");
    }

    #[test]
    fn a_trimmed_item_says_so_on_its_line_and_a_single_update_shows_no_count() {
        let history = public_history(
            &delivery_history(
                "quiet",
                json!({
                    "items": [
                        {
                            "instance": "calendar:fixture-run-3",
                            "updates": 60,
                            "seq": "941→1000",
                            "trimmed": [{"path": "/candles", "total": 1000, "kept": 25, "rule": "newest_last"}],
                            "data": {"candles": []},
                        },
                        {"instance": "calendar:fixture-run-4", "updates": 1, "seq": "7", "data": {}},
                    ],
                }),
            ),
            DisplayProfile::default(),
        );

        assert!(
            history.contains("◉ TickUpdate: Tick fixture-  #941→1000 · 60 merged · trimmed"),
            "{history}"
        );
        assert!(
            history.contains("◉ TickUpdate: Tick fixture-  #7\n"),
            "{history}"
        );
    }

    #[test]
    fn a_delivery_recorded_before_update_counts_still_shows_its_instance() {
        let history = public_history(
            &delivery_history(
                "quiet",
                json!({
                    "items": [{"instance": "calendar:fixture-run-3", "sequence": 12, "dropped": 4, "data": null}],
                    "truncated": true,
                }),
            ),
            DisplayProfile::default(),
        );

        assert!(
            history.contains("◉ TickUpdate: Tick fixture-\n"),
            "{history}"
        );
    }

    #[test]
    fn a_short_reply_to_a_tick_delivery_folds_into_one_muted_line() {
        let folded = public_history(
            &tick_delivery_history("Nothing notable.\nStill in range."),
            DisplayProfile::default(),
        );
        assert!(
            folded.contains("  Nothing notable. Still in range.\n"),
            "{folded}"
        );
        assert!(!folded.contains("⏺ Nothing notable"), "{folded}");

        let long = "x".repeat(FOLDED_TICK_REPLY_CHARACTERS + 1);
        let expanded = public_history(&tick_delivery_history(&long), DisplayProfile::default());
        assert!(expanded.contains(&format!("⏺ {long}")), "{expanded}");
    }

    #[test]
    fn resumed_prompts_link_cached_images_and_show_expired_ones_as_plain_text() {
        let root = tempfile::tempdir().unwrap();
        let chat = Uuid::new_v4();
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(3, 2))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let cache = crate::terminal::image_cache::ImageCache::in_root(root.path());
        let cached = cache
            .store(
                chat,
                1,
                &tickloop_harness::base::image::prepare(&png).unwrap(),
            )
            .unwrap();
        let snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": chat,
            "folder": "/workspace",
            "messages": [
                user_text("compare [Image #1] with [Image #2]"),
                Message {
                    role: Role::Assistant,
                    content: vec![Content::Text { text: "[Image #1] is wider".into() }],
                },
            ],
            "chat_loop": {"id":"00000000-0000-0000-0000-000000000000","state":"Idle"},
        }))
        .unwrap();
        let linked_runs = || {
            public_history_pieces(
                &snapshot,
                DisplayProfile::default(),
                ImageLinks::new(Some(cache.clone()), true),
                &AgentColors::default(),
            )
            .into_iter()
            .flat_map(|piece| match piece {
                ConversationPiece::Input { spans, .. } => spans
                    .into_iter()
                    .filter_map(|span| span.link.map(|link| (span.text, link)))
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>()
        };

        assert_eq!(
            linked_runs(),
            [(
                "[Image #1]".to_owned(),
                super::super::hyperlink::Hyperlink::file(&cached).unwrap()
            )],
            "only the prompt's cached image is linked"
        );
        std::fs::remove_dir_all(root.path().join("image-cache")).unwrap();
        assert!(
            linked_runs().is_empty(),
            "an expired cache shows plain text"
        );
        assert!(
            public_history(&snapshot, DisplayProfile::default())
                .contains("❯ compare [Image #1] with [Image #2]")
        );
    }

    #[test]
    fn resumed_history_hides_interruption_placeholders() {
        let snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": Uuid::from_u128(600),
            "folder": "/workspace",
            "messages": [
                user_text("run the slow command"),
                Message {
                    role: Role::Assistant,
                    content: vec![Content::ToolUse {
                        id: "slow-call".into(),
                        name: "Bash".into(),
                        input: json!({"command": "sleep 20"}),
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![Content::ToolResult {
                        tool_use_id: "slow-call".into(),
                        content: json!({"output": "partial"}).to_string().into(),
                        is_error: false,
                    }],
                },
                user_text("Continue from where you left off."),
                Message {
                    role: Role::Assistant,
                    content: vec![Content::Text {
                        text: "No response requested.".into(),
                    }],
                },
            ],
            "chat_loop": {"id":"00000000-0000-0000-0000-000000000000","state":"Idle"},
        }))
        .unwrap();

        let history = public_history(&snapshot, DisplayProfile::default());
        assert!(history.contains("❯ run the slow command"));
        assert!(!history.contains("Continue from where you left off."));
        assert!(!history.contains("No response requested."));
    }

    #[tokio::test]
    async fn images_read_by_tools_are_shown_as_their_text_summary_only() {
        let directory = tempfile::tempdir().unwrap();
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(40, 30, |x, y| {
            image::Rgb([x as u8 * 6, y as u8 * 8, 120])
        }))
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
        std::fs::write(directory.path().join("chart.png"), &bytes).unwrap();
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
            tickloop_harness::test_support::call(
                "read-chart",
                "Read",
                json!({"file_path": directory.path().join("chart.png")}),
            ),
            tickloop_harness::test_support::text("A chart."),
        ]));
        let agent = Agent::open_embedded(
            directory.path().join("state"),
            tickloop_harness::test_support::config(),
            model,
        )
        .unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Describe chart.png".into())
            .await
            .unwrap();
        let mut compact = super::presentation::ToolPresentation::new(false);
        let detailed = super::presentation::ToolPresentation::new(true);
        let mut rendered = Vec::new();
        loop {
            let event = tickloop_harness::test_support::next_event(&mut events).await;
            compact.record(&event);
            if matches!(event.kind, EventKind::ToolCompleted { .. }) {
                rendered.extend(compact.render_event(&event));
                rendered.extend(detailed.render_event(&event));
            }
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let snapshot = agent.snapshot(chat).await.unwrap();
        rendered.push(public_history(&snapshot, DisplayProfile::default()));
        rendered.push(
            super::presentation::transcript_document(
                &snapshot,
                DisplayProfile::default(),
                &ImageLinks::default(),
                &AgentColors::default(),
            )
            .plain_text(200),
        );

        let base64_prefix = &BASE64.encode(&bytes)[..32];
        assert_eq!(rendered.len(), 4);
        for view in rendered {
            assert!(view.contains("chart.png"), "{view}");
            assert!(view.contains("image/png"), "{view}");
            assert!(!view.contains(base64_prefix), "{view}");
        }
        agent.shutdown().await;
    }

    #[test]
    fn from_here_history_keeps_prefix_tool_and_user_shell_rows() {
        let history = public_history(
            &directed_history_fixture(SummaryDirection::FromHere),
            DisplayProfile::default(),
        );
        for retained in [
            "prefix prompt",
            "Read · prefix.txt",
            "prefix tool output",
            "! prefix-shell",
            "⎿  prefix shell output",
            "prefix answer",
            "Summarized conversation (ctrl+o to expand)",
        ] {
            assert!(history.contains(retained), "missing {retained}: {history}");
        }
        assert!(
            !history.contains("suffix summary"),
            "the summary itself is in the detailed transcript: {history}"
        );
        for summarized in [
            "selected prompt",
            "printf tail",
            "tail tool output",
            "! tail-shell",
            "tail shell output",
            "tail answer",
        ] {
            assert!(
                !history.contains(summarized),
                "retained {summarized}: {history}"
            );
        }
        assert!(
            history.find("prefix answer").unwrap()
                < history.find("Summarized conversation").unwrap()
        );
    }

    #[test]
    fn compacted_history_keeps_earlier_rows_above_the_summary_marker() {
        let history = public_history(&full_compaction_fixture(), DisplayProfile::default());
        let marker = history
            .find("Summarized conversation (ctrl+o to expand)")
            .unwrap_or_else(|| panic!("missing summary marker: {history}"));
        for earlier in [
            "prefix prompt",
            "Read · prefix.txt",
            "prefix answer",
            "selected prompt",
            "tail answer",
            "⎿  tail shell output",
        ] {
            let position = history
                .find(earlier)
                .unwrap_or_else(|| panic!("missing {earlier}: {history}"));
            assert!(position < marker, "{earlier} after the marker: {history}");
        }
        assert!(history.find("after compaction prompt").unwrap() > marker);
        assert!(!history.contains("full summary"), "{history}");
        assert!(!history.contains("Conversation compacted"), "{history}");
    }

    #[test]
    fn restored_history_shows_the_prompt_reply_and_tool_marks() {
        let history = public_history(
            &directed_history_fixture(SummaryDirection::FromHere),
            DisplayProfile::default(),
        );

        assert!(history.contains("❯ prefix prompt"), "{history}");
        assert!(history.contains("⏺ prefix answer"), "{history}");
        assert!(history.contains("⏺ Read"), "{history}");
        assert!(history.contains("⎿  Completed"), "{history}");
        assert!(history.contains("⎿  prefix shell output"), "{history}");
    }

    #[test]
    fn up_to_here_history_keeps_suffix_tool_and_user_shell_rows() {
        let history = public_history(
            &directed_history_fixture(SummaryDirection::UpToHere),
            DisplayProfile::default(),
        );
        assert!(
            !history.contains("prefix summary"),
            "the summary itself is in the detailed transcript: {history}"
        );
        for retained in [
            "Summarized conversation (ctrl+o to expand)",
            "selected prompt",
            "Bash · printf tail",
            "tail tool output",
            "! tail-shell",
            "⎿  tail shell output",
            "tail answer",
        ] {
            assert!(history.contains(retained), "missing {retained}: {history}");
        }
        for summarized in [
            "prefix prompt",
            "prefix.txt",
            "prefix tool output",
            "! prefix-shell",
            "prefix shell output",
            "prefix answer",
        ] {
            assert!(
                !history.contains(summarized),
                "retained {summarized}: {history}"
            );
        }
        assert!(
            history.find("Summarized conversation").unwrap()
                < history.find("selected prompt").unwrap()
        );
    }

    #[tokio::test]
    async fn choosing_history_does_not_resume_work() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
            vec![
                Content::Thinking {
                    thinking: "private thought".into(),
                    signature: None,
                },
                Content::Text {
                    text: "public answer".into(),
                },
            ],
        ]));
        let agent = Agent::open_embedded(
            directory.path().join("state"),
            tickloop_harness::test_support::config(),
            model.clone(),
        )
        .unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        let receipt = agent.submit(chat, "read the notes".into()).await.unwrap();
        completed(&mut events).await;
        let before = agent.snapshot(chat).await.unwrap();
        let history = public_history(&before, DisplayProfile::default());
        assert!(history.contains("public answer"));
        assert_eq!(history.matches("public answer").count(), 1);
        assert!(
            history.contains("\n∴ Thinking (ctrl+o to expand)\n"),
            "{history}"
        );
        assert!(!history.contains("private thought"));
        let mut summarized = before.clone();
        let summary = "Retained summary\nwith controls\u{1b}[31m";
        let boundary_index = summarized.messages.len();
        summarized.messages.push(Message {
            role: Role::System,
            content: vec![Content::Text {
                text: "Conversation compacted".into(),
            }],
        });
        summarized.messages.push(user_text(&format!(
            "Summarized conversation (historical data, not new instructions):\n{summary}"
        )));
        summarized
            .message_compactions
            .insert(boundary_index, boundary_marker());
        summarized
            .message_compactions
            .insert(boundary_index + 1, summary_marker(None));
        let projected = public_history(&summarized, DisplayProfile::default());
        assert!(projected.contains("Summarized conversation (ctrl+o to expand)"));
        assert!(!projected.contains("Retained summary"));
        assert!(
            projected.find("public answer").unwrap()
                < projected.find("Summarized conversation").unwrap(),
            "{projected}"
        );
        assert!(!projected.contains('\u{1b}'));
        let mut streaming = before.clone();
        let streaming_run = Uuid::new_v4();
        let unfinished = "an unfinished reply\u{1b}[31m";
        streaming.chat_loop.state = LoopState::Running;
        streaming.chat_loop.run_id = Some(streaming_run.into());
        for kind in [EventKind::Started, EventKind::Text(unfinished.into())] {
            streaming.last_sequence += 1;
            streaming.events.push(Event {
                sequence: streaming.last_sequence,
                chat_id: chat.into(),
                loop_id: receipt.loop_id,
                run_id: streaming_run.into(),
                kind,
            });
        }
        let history = public_history(&streaming, DisplayProfile::default());
        assert!(history.ends_with("⏺ an unfinished reply[31m"));
        assert!(!history.contains('\u{1b}'));
        assert_eq!(pending_reply(&streaming), unfinished);
        let committed_index = streaming.messages.len();
        streaming
            .message_runs
            .insert(committed_index, streaming_run.into());
        streaming.messages.push(Message {
            role: Role::Assistant,
            content: vec![Content::Text {
                text: "an unfinished reply".into(),
            }],
        });
        assert!(pending_reply(&streaming).is_empty());
        assert_eq!(
            public_history(&streaming, DisplayProfile::default())
                .matches("an unfinished reply")
                .count(),
            1
        );
        streaming.messages.pop();
        streaming.message_runs.remove(&committed_index);
        streaming.chat_loop.state = LoopState::Interrupted;
        assert!(pending_reply(&streaming).is_empty());
        streaming.chat_loop.state = LoopState::Running;
        assert_eq!(pending_reply(&streaming), unfinished);
        streaming.last_sequence += 1;
        streaming.events.push(Event {
            sequence: streaming.last_sequence,
            chat_id: chat.into(),
            loop_id: receipt.loop_id,
            run_id: receipt.run_id,
            kind: EventKind::ConversationRewound {
                message_id: Uuid::new_v4(),
            },
        });
        assert!(pending_reply(&streaming).is_empty());
        let (console, _) = Console::start(false).unwrap();
        let (_, picker) = Workflow::chats(&agent, directory.path(), None, false)
            .await
            .unwrap();
        let selected = picker.choose(&agent, &console, Some(0)).await.unwrap();
        assert_eq!(selected.selection, Some(chat));
        assert!(selected.receipt.is_none());
        assert_eq!(selected.restored.as_ref().unwrap().id, chat);
        assert_eq!(
            selected.restored.unwrap().last_sequence,
            before.last_sequence
        );
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        assert_eq!(
            agent.snapshot(chat).await.unwrap().last_sequence,
            before.last_sequence
        );
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn shell_tasks_keep_user_and_agent_controls_separate() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
            tickloop_harness::test_support::call(
                "agent-shell",
                "Bash",
                json!({"command":"sleep 30","run_in_background":true}),
            ),
            tickloop_harness::test_support::text("Agent task is running in the background."),
        ]));
        let agent = Agent::new_embedded(tickloop_harness::test_support::config(), model);
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::AcceptEdits)
            .await
            .unwrap();
        agent
            .set_permission_rules(
                chat,
                vec![PermissionRule {
                    id: Uuid::new_v4(),
                    decision: Decision::Allow,
                    tool: "Bash".into(),
                    specifier: None,
                    input: None,
                    loop_id: None,
                    persistent: false,
                }],
            )
            .await
            .unwrap();
        let mut agent_events = agent.subscribe();
        agent
            .submit(chat, "Start an Agent Shell task.".into())
            .await
            .unwrap();
        loop {
            match tickloop_harness::test_support::next_event(&mut agent_events)
                .await
                .kind
            {
                EventKind::PermissionRequested { request } => agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap(),
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("Agent Shell task failed: {error}"),
                _ => {}
            }
        }

        let mut user_events = agent.subscribe_user_shells();
        let user = agent
            .submit_user_shell(chat, "sleep 30".into(), false)
            .await
            .unwrap();
        user_shell_reaches(&mut user_events, user.command_id, UserShellState::Running).await;

        let (tasks_menu, tasks) = Workflow::commands(&agent, chat).await.unwrap();
        assert!(tasks_menu.options[0].starts_with("User · sleep 30 · Running"));
        assert!(tasks_menu.options[1].starts_with("Agent · sleep 30 · Background"));
        let Workflow::Commands { choices, .. } = &tasks else {
            panic!("expected Shell tasks workflow")
        };
        assert!(matches!(choices[0], CommandChoice::User(id) if id == user.command_id));
        assert!(matches!(choices[1], CommandChoice::Agent(_)));

        let (console, _) = Console::start(false).unwrap();
        let mut user_task = tasks
            .choose(&agent, &console, Some(0))
            .await
            .unwrap()
            .next
            .unwrap();
        assert_eq!(user_task.0.title, "User Shell task");
        assert!(
            user_task
                .0
                .context
                .iter()
                .any(|line| line == "Origin: User")
        );
        let background = user_task
            .0
            .options
            .iter()
            .position(|option| option == "Move User task to background")
            .unwrap();
        user_task = user_task
            .1
            .choose(&agent, &console, Some(background))
            .await
            .unwrap()
            .next
            .unwrap();
        assert!(
            user_task
                .0
                .context
                .iter()
                .any(|line| line == "Status: Background")
        );
        let output = user_task
            .0
            .options
            .iter()
            .position(|option| option == "View output from the beginning")
            .unwrap();
        user_task = user_task
            .1
            .choose(&agent, &console, Some(output))
            .await
            .unwrap()
            .next
            .unwrap();
        let stop = user_task
            .0
            .options
            .iter()
            .position(|option| option == "Stop this User task")
            .unwrap();
        user_task
            .1
            .choose(&agent, &console, Some(stop))
            .await
            .unwrap();
        assert!(matches!(
            agent
                .snapshot(chat)
                .await
                .unwrap()
                .user_shells
                .iter()
                .find(|record| record.id == user.command_id)
                .unwrap()
                .state,
            UserShellState::Cancelling | UserShellState::Failed
        ));

        let (tasks_menu, tasks) = Workflow::commands(&agent, chat).await.unwrap();
        let agent_index = tasks_menu
            .options
            .iter()
            .position(|option| option.starts_with("Agent · sleep 30"))
            .unwrap();
        let mut agent_task = tasks
            .choose(&agent, &console, Some(agent_index))
            .await
            .unwrap()
            .next
            .unwrap();
        assert_eq!(agent_task.0.title, "Agent Shell task");
        assert!(
            agent_task
                .0
                .context
                .iter()
                .any(|line| line == "Origin: Agent")
        );
        let output = agent_task
            .0
            .options
            .iter()
            .position(|option| option == "View output from the beginning")
            .unwrap();
        agent_task = agent_task
            .1
            .choose(&agent, &console, Some(output))
            .await
            .unwrap()
            .next
            .unwrap();
        let stop = agent_task
            .0
            .options
            .iter()
            .position(|option| option == "Stop this Agent task")
            .unwrap();
        agent_task
            .1
            .choose(&agent, &console, Some(stop))
            .await
            .unwrap();
        assert!(matches!(
            agent
                .commands(chat)
                .await
                .unwrap()
                .into_iter()
                .find(|command| command.command == "sleep 30")
                .unwrap()
                .state,
            CommandState::Cancelling | CommandState::Failed
        ));
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn plan_selection_approves_only_reviewed_version_then_continues_same_run() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
            tickloop_harness::test_support::submit_plan("p1", "Read notes first."),
            tickloop_harness::test_support::submit_plan("p2", "Read notes and summarize."),
            tickloop_harness::test_support::text("Approved implementation finished."),
        ]));
        let agent = Agent::new_embedded(tickloop_harness::test_support::config(), model.clone());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Plan)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        let run = agent.submit(chat, "Prepare a plan.".into()).await.unwrap();
        async fn approval(
            events: &mut tokio::sync::broadcast::Receiver<tickloop_harness::protocol::Event>,
        ) {
            loop {
                match tickloop_harness::test_support::next_event(events)
                    .await
                    .kind
                {
                    EventKind::PermissionRequested { request }
                        if request.name == tickloop_harness::tools::EXIT_PLAN_MODE_TOOL_NAME =>
                    {
                        return;
                    }
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
        }
        approval(&mut events).await;
        let (stale_menu, stale) = Workflow::plan(&agent, chat).await.unwrap();
        assert_eq!(
            stale_menu.options,
            [
                "Approve and start · Auto",
                "Approve and start · Ask",
                "Approve and start · Full Access",
                "No, keep planning"
            ]
        );
        let (console, _) = Console::start(false).unwrap();
        let (_, review) = Workflow::plan(&agent, chat).await.unwrap();
        assert!(
            review
                .choose_with_feedback(&agent, &console, Some(3), Some("Add the summary.".into()))
                .await
                .unwrap()
                .receipt
                .is_none()
        );
        approval(&mut events).await;
        assert!(
            stale
                .choose(&agent, &console, Some(0))
                .await
                .err()
                .unwrap()
                .contains("revised")
        );
        assert_eq!(model.requests.lock().unwrap().len(), 2);
        let (_, current) = Workflow::plan(&agent, chat).await.unwrap();
        let effect = current.choose(&agent, &console, Some(0)).await.unwrap();
        assert!(effect.receipt.is_none());
        completed(&mut events).await;
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.plans[0].version, 2);
        assert_eq!(
            snapshot.plans[0].approved_execution_mode,
            Some(ExecutionMode::Auto)
        );
        assert_eq!(snapshot.chat_loop.run_id, Some(run.run_id));
        let requests = model.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::ToolResult {tool_use_id, content, is_error:false} if tool_use_id == "p2" && content.text().contains("User has approved your plan.") && content.text().ends_with("Read notes and summarize."))));
        let (saved, _) = Workflow::plan(&agent, chat).await.unwrap();
        assert_eq!(saved.options, ["Close"]);
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn chat_picker_lists_recent_chats_with_titles_and_branch_sources() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
            tickloop_harness::test_support::text("source answer"),
        ]));
        let agent = Agent::open_embedded(
            root.clone(),
            tickloop_harness::test_support::config(),
            model,
        )
        .unwrap();
        let source = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        agent.submit(source, "source prompt".into()).await.unwrap();
        completed(&mut events).await;
        let child = agent
            .branch_chat(BranchRequest {
                request_id: Uuid::new_v4(),
                source_chat_id: source.into(),
                name: Some("child".into()),
            })
            .await
            .unwrap()
            .child_chat_id;
        agent.flush_transcripts().await;
        let now = SystemTime::now();
        for (chat, age_seconds) in [(source, 7_200), (child.into(), 60)] {
            std::fs::File::options()
                .write(true)
                .open(tickloop_harness::test_support::transcript_file(&root, chat))
                .unwrap()
                .set_modified(now - std::time::Duration::from_secs(age_seconds))
                .unwrap();
        }

        let (picker, workflow) = Workflow::chats(&agent, directory.path(), None, false)
            .await
            .unwrap();
        let rows = picker
            .entries()
            .iter()
            .map(|entry| entry.plain_row())
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                "child (Branch) · 1m ago · Branch of source prompt".to_owned(),
                "source prompt · 2h ago".to_owned(),
            ]
        );
        assert!(
            rows.iter()
                .all(|row| !row.contains(&child.uuid().to_string())
                    && !row.contains(&source.to_string())),
            "{rows:?}"
        );
        let Workflow::Chats {
            choices,
            as_new_chat,
        } = workflow
        else {
            panic!("chat picker returned the wrong workflow");
        };
        assert_eq!(choices, vec![child, source.into()]);
        assert!(!as_new_chat);

        let (picker, workflow) =
            Workflow::chats(&agent, directory.path(), Some(child.into()), true)
                .await
                .unwrap();
        assert_eq!(picker.title(), "Copy Chat");
        assert_eq!(picker.entries().len(), 1);
        assert!(
            matches!(workflow, Workflow::Chats { choices, as_new_chat: true } if choices == vec![source])
        );

        std::fs::remove_file(tickloop_harness::test_support::transcript_file(
            &root, source,
        ))
        .unwrap();
        let (picker, workflow) = Workflow::chats(&agent, directory.path(), None, false)
            .await
            .unwrap();
        assert_eq!(
            picker.entries()[0].plain_row(),
            "child (Branch) · 1m ago",
            "an unknown Branch source is omitted, never named by its ID"
        );
        assert!(matches!(workflow, Workflow::Chats { choices, .. } if choices == vec![child]));
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            Workflow::chats(&agent, empty.path(), None, false)
                .await
                .err()
                .as_deref(),
            Some("No conversations found to resume.")
        );
        agent.shutdown().await;
    }
}

use std::collections::HashMap;
use std::time::SystemTime;

use tickloop_agent::{
    model::{Content, Role},
    permissions::ExecutionMode,
    runtime::{
        Agent, BranchPublicItem, BranchPublicSource, ChatSnapshot, EventKind,
        FOLDED_TICK_REPLY_CHARACTERS, InputContext, LocalMessage, LoopState, Receipt, SavedChat,
        UserShellRecord, UserShellState,
    },
    tools::{CommandSnapshot, CommandState, ToolResult},
};
use uuid::Uuid;

use super::{
    console::{Console, ConversationPiece, Menu},
    conversation, directed_summary,
    display::DisplayProfile,
    hyperlink,
    image_cache::ImageLinks,
    presentation, safe_text,
};

const COMMAND_OUTPUT_PAGE_BYTES: usize = 16 * 1024;
const PROCESS_LOCAL_TASKS_NOTE: &str =
    "Shell tasks and their output are available only in this process; a restart clears them.";

pub(super) enum Workflow {
    Chats {
        choices: Vec<Uuid>,
        as_new_chat: bool,
    },
    Goals {
        chat: Uuid,
        choices: Vec<Uuid>,
    },
    Goal {
        chat: Uuid,
        goal: Uuid,
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
        goal: Uuid,
        version: u64,
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
    pub(super) selection: Option<(Uuid, Option<Uuid>)>,
    pub(super) receipt: Option<Receipt>,
    pub(super) restored: Option<ChatSnapshot>,
    pub(super) timer: Option<(Uuid, Uuid)>,
}

impl Workflow {
    pub(super) fn chat(&self) -> Option<Uuid> {
        match self {
            Self::Chats { .. } => None,
            Self::Goals { chat, .. }
            | Self::Goal { chat, .. }
            | Self::Commands { chat, .. }
            | Self::Command { chat, .. }
            | Self::Plan { chat, .. } => Some(*chat),
        }
    }

    /// Chat 选择器：列出本项目已保存的 Chat（不含当前前台 Chat），按最近活动排序，
    /// 每行显示标题或首条提示、相对时间、git 分支与 Branch 来源。`as_new_chat` 时选中后复制成新 Chat（`--fork-session`）。
    pub(super) async fn chats(
        agent: &Agent,
        workdir: &std::path::Path,
        current_chat: Option<Uuid>,
        as_new_chat: bool,
    ) -> Result<(Menu, Self), String> {
        let saved = agent
            .saved_chats(workdir)
            .await
            .map_err(|error| error.to_string())?;
        let chats = saved
            .iter()
            .filter(|chat| Some(chat.id) != current_chat)
            .collect::<Vec<_>>();
        if chats.is_empty() {
            return Err("No conversations found to resume.".into());
        }
        let options = chats
            .iter()
            .map(|chat| {
                let mut row = format!(
                    "{} · {}",
                    saved_chat_title(chat),
                    activity_age(chat.modified)
                );
                if let Some(branch) = &chat.git_branch {
                    row.push_str(&format!(" · {}", single_line(branch)));
                }
                if let Some(source) = chat.branch_source {
                    let source_title = saved
                        .iter()
                        .find(|candidate| candidate.id == source)
                        .map_or_else(|| source.to_string(), saved_chat_title);
                    row.push_str(&format!(" · Branch of {source_title}"));
                }
                row.push_str(&format!(" · {}", chat.id));
                row
            })
            .chain(std::iter::once("Cancel".into()))
            .collect();
        let context = if as_new_chat {
            "Choose a conversation to copy into a new Chat. The original conversation stays unchanged."
        } else {
            "Choose a conversation to resume. Opening it shows its history and starts no work."
        };
        Ok((
            menu("Resume Chat", vec![context.into()], options),
            Self::Chats {
                choices: chats.into_iter().map(|chat| chat.id).collect(),
                as_new_chat,
            },
        ))
    }

    pub(super) async fn goals(agent: &Agent, chat: Uuid) -> Result<(Menu, Self), String> {
        let snapshot = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        let options = snapshot
            .loops
            .iter()
            .map(|goal| format!("{} · {:?}", single_line(&goal.goal), goal.state))
            .chain(std::iter::once("New goal".into()))
            .chain(std::iter::once("Cancel".into()))
            .collect();
        let context = if snapshot.loops.is_empty() {
            "Choose New goal to start another goal with your next input, keeping this Chat's history."
        } else {
            "Choose a goal to view its trigger, configure a timer or select it for your next message."
        };
        Ok((
            menu("Loops", vec![context.into()], options),
            Self::Goals {
                chat,
                choices: snapshot.loops.iter().map(|goal| goal.id).collect(),
            },
        ))
    }

    pub(super) async fn goal(
        agent: &Agent,
        chat: Uuid,
        goal: Uuid,
    ) -> Result<(Menu, Self), String> {
        let snapshot = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        let selected = snapshot
            .loops
            .iter()
            .find(|item| item.id == goal)
            .ok_or("The selected goal no longer exists.")?;
        let mut context = vec![
            format!("{} · {:?}", single_line(&selected.goal), selected.state),
            format!("Goal: {goal}"),
        ];
        context.extend(super::loops::trigger_lines(selected.trigger.as_ref()));
        Ok((
            menu(
                "Loop details",
                context,
                vec![
                    "Use this goal".into(),
                    "Configure timer · edit command".into(),
                    "Back to Loops".into(),
                ],
            ),
            Self::Goal { chat, goal },
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

    pub(super) async fn plan(
        agent: &Agent,
        chat: Uuid,
        goal: Uuid,
    ) -> Result<(Menu, Self), String> {
        let snapshot = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        let selected_goal = snapshot
            .loops
            .iter()
            .find(|entry| entry.id == goal)
            .ok_or("The selected goal no longer exists.")?;
        let plan = snapshot
            .plans
            .iter()
            .find(|plan| plan.loop_id == goal)
            .ok_or("This goal has no plan yet. Ask the Agent to create one in Plan mode.")?;
        let mut context = vec![
            format!("Goal: {}", safe_text(&selected_goal.goal)),
            format!("Plan version: {}", plan.version),
        ];
        if let Some(mode) = plan.approved_execution_mode {
            context.push(format!("Already approved with mode: {mode}"));
        }
        context.extend(safe_text(&plan.markdown).split('\n').map(str::to_owned));
        context.push(
            "Approve and start submits this goal for execution using the reviewed version.".into(),
        );
        let mut menu = menu(
            "Review plan",
            context,
            vec![
                "Approve and start · Auto".into(),
                "Approve and start · Ask".into(),
                "Approve and start · Full Access".into(),
                "Keep planning".into(),
            ],
        );
        menu.confirmation = true;
        Ok((
            menu,
            Self::Plan {
                chat,
                goal,
                version: plan.version,
            },
        ))
    }

    pub(super) async fn choose(
        self,
        agent: &Agent,
        console: &Console,
        index: Option<usize>,
    ) -> Result<WorkflowEffect, String> {
        let Some(index) = index else {
            let mut effect = WorkflowEffect::default();
            if let Self::Goal { chat, .. } = self {
                effect.next = Some(Self::goals(agent, chat).await?);
            }
            return Ok(effect);
        };
        let mut effect = WorkflowEffect::default();
        match self {
            Self::Chats {
                choices,
                as_new_chat,
            } => {
                if index == choices.len() {
                    return Ok(effect);
                }
                let chosen = *choices.get(index).ok_or("Invalid Chat selection.")?;
                let snapshot = if as_new_chat {
                    agent.resume_as_new_chat(chosen).await
                } else {
                    agent.resume_chat(chosen).await
                }
                .map_err(|error| error.to_string())?;
                let chat = snapshot.id;
                console.reset_chat(chat)?;
                console.write(&format!(
                    "\nOpened Chat: {} · {}\n",
                    chat_title(&snapshot),
                    snapshot.id,
                ))?;
                if let Some(origin) = &snapshot.branch_origin {
                    console.write(&format!("Branch of Chat: {}\n", origin.source_chat_id))?;
                }
                if as_new_chat {
                    console.write(&format!("Copied from Chat: {chosen}\n"))?;
                }
                for warning in agent.transcript_load_warnings(chat) {
                    super::write_warning(console, &warning)?;
                }
                super::show_settings_warnings(console, agent.user_settings_warnings(chat))?;
                console.write("History only; selecting this Chat starts no work.\n")?;
                write_public_history(console, &snapshot)?;
                let goal = if snapshot.loops.len() == 1 {
                    Some(snapshot.loops[0].id)
                } else {
                    None
                };
                effect.selection = Some((chat, goal));
                if snapshot.loops.len() > 1 {
                    effect.next = Some(Self::goals(agent, chat).await?);
                }
                effect.restored = Some(snapshot);
            }
            Self::Goals { chat, choices } => {
                if index == choices.len() {
                    console.write("\nThe next input will start a new goal in this Chat.\n")?;
                    effect.selection = Some((chat, None));
                    return Ok(effect);
                }
                if index == choices.len() + 1 {
                    return Ok(effect);
                }
                let goal = *choices.get(index).ok_or("Invalid goal selection.")?;
                effect.next = Some(Self::goal(agent, chat, goal).await?);
            }
            Self::Goal { chat, goal } => {
                match index {
                    1 => {
                        effect.timer = Some((chat, goal));
                        return Ok(effect);
                    }
                    2 => {
                        effect.next = Some(Self::goals(agent, chat).await?);
                        return Ok(effect);
                    }
                    0 => {}
                    _ => return Err("Invalid goal action.".into()),
                }
                let snapshot = agent
                    .snapshot(chat)
                    .await
                    .map_err(|error| error.to_string())?;
                let selected = snapshot
                    .loops
                    .iter()
                    .find(|entry| entry.id == goal)
                    .ok_or("The selected goal no longer exists.")?;
                let next_step = match selected.state {
                    LoopState::Stopped | LoopState::Expired => {
                        "This goal has ended. Use /loops new to start another goal."
                    }
                    LoopState::WaitingPermission => {
                        "This goal is waiting for your approval in the permission menu."
                    }
                    _ => "Enter a message when you want this goal to continue.",
                };
                console.write(&format!(
                    "\nSelected goal: {} · {:?}\n{next_step}\n",
                    safe_text(&selected.goal),
                    selected.state,
                ))?;
                effect.selection = Some((chat, Some(goal)));
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
            Self::Plan {
                chat,
                goal,
                version,
            } => {
                let mode = match index {
                    0 => ExecutionMode::Auto,
                    1 => ExecutionMode::AcceptEdits,
                    2 => ExecutionMode::FullAccess,
                    3 => {
                        console.write("\nPlan kept for review. Enter the revisions you want.\n")?;
                        return Ok(effect);
                    }
                    _ => return Err("Invalid plan selection.".into()),
                };
                let approval = agent
                    .approve_plan(chat, goal, version, mode)
                    .await
                    .map_err(|error| error.to_string())?;
                let receipt=agent.submit(chat,Some(goal),approval).await
                    .map_err(|error|format!("Plan version {version} was approved, but execution could not start: {error}"))?;
                console.write(&format!("\nPlan version {version} approved. Starting this goal with {mode} permissions.\n"))?;
                effect.selection = Some((chat, Some(goal)));
                effect.receipt = Some(receipt);
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
        id: Uuid::new_v4(),
        title: title.into(),
        context,
        options,
        selected: 0,
        confirmation: false,
    }
}

pub(super) fn chat_title(chat: &ChatSnapshot) -> String {
    if let Some(name) = chat.branch_name() {
        return single_line(name);
    }
    chat.loops
        .first()
        .map(|goal| single_line(&goal.goal))
        .unwrap_or_else(|| "New conversation".into())
}

/// 选择器标题：Branch 名称或自定义标题，其次首条提示。
pub(super) fn saved_chat_title(chat: &SavedChat) -> String {
    chat.title
        .as_deref()
        .or(chat.first_prompt.as_deref())
        .map(single_line)
        .unwrap_or_else(|| "New conversation".into())
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

/// 当前 Run 已流出但尚未写入对话的文字，只由运行期事件推出，不随 Chat 保存。
pub(super) fn pending_reply(snapshot: &ChatSnapshot) -> String {
    let visible_events = snapshot.events.iter().collect::<Vec<_>>();
    let Some(streaming_run) = visible_events
        .last()
        .filter(|event| matches!(event.kind, EventKind::Text(_)))
        .map(|event| event.run_id)
    else {
        return String::new();
    };
    let is_running = snapshot
        .loops
        .iter()
        .any(|goal| goal.run_id == Some(streaming_run) && goal.state == LoopState::Running);
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

#[derive(Default)]
struct PublicHistoryBuilder {
    pieces: Vec<ConversationPiece>,
    image_links: ImageLinks,
}

impl PublicHistoryBuilder {
    fn push_str(&mut self, text: &str) {
        self.push(text.to_owned(), None);
    }

    fn push_role(&mut self, text: String, role: super::style::Role) {
        self.push(text, Some(role));
    }

    /// Prompt text of `chat`, with each cached `[Image #N]` linked to its image.
    fn push_prompt(&mut self, chat: Uuid, text: &str) {
        let links = self.image_links.in_text(chat, text);
        for (run, link) in hyperlink::runs(text, &links) {
            match link {
                Some(link) => self.pieces.push(ConversationPiece::Linked {
                    text: run.to_owned(),
                    link: link.clone(),
                }),
                None => self.push_str(run),
            }
        }
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
) -> Vec<ConversationPiece> {
    let mut history = PublicHistoryBuilder {
        pieces: Vec::new(),
        image_links,
    };
    let Ok(projection) = chat.public_conversation_items() else {
        return vec![ConversationPiece::Styled(
            "\nConversation view unavailable. Ctrl+O opens the full archive.\n".into(),
            None,
        )];
    };
    let mut tool_calls = HashMap::new();
    let mut answers_tick_delivery = false;
    for item in projection {
        match item {
            BranchPublicItem::Summary { summary, .. } => {
                history.push_str(&directed_summary::history_marker(&summary));
                answers_tick_delivery = false;
            }
            BranchPublicItem::Message {
                source,
                message,
                contexts,
                has_thinking,
            } => {
                if has_thinking {
                    history.push_role(
                        "\n∴ Thinking (ctrl+o to expand)\n".into(),
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
            }
        }
    }
    let pending = pending_reply(chat);
    if !pending.is_empty() {
        history.push_str("\n");
        history.push_reply(&safe_text(&pending));
    }
    history.pieces
}

/// The history as plain text, with each reply's Markdown source behind the
/// assistant glyph.
#[cfg(test)]
pub(super) fn public_history(chat: &ChatSnapshot, display: DisplayProfile) -> String {
    let assistant_lead = format!("{} ", display.glyph(super::display::Glyph::Assistant));
    public_history_pieces(chat, display, ImageLinks::default())
        .iter()
        .map(|piece| match piece {
            ConversationPiece::Styled(text, _) | ConversationPiece::Linked { text, .. } => {
                text.clone()
            }
            ConversationPiece::Markdown { text, .. } => format!("{assistant_lead}{text}"),
        })
        .collect()
}

pub(super) fn write_public_history(console: &Console, chat: &ChatSnapshot) -> Result<(), String> {
    let pieces = public_history_pieces(chat, console.display_profile()?, console.image_links()?);
    console.write_pieces(&pieces)
}

/// A Tick Update delivery: the Chat shows its `◉` lines, never the payload the model reads.
fn is_tick_delivery(message: &tickloop_agent::model::Message) -> bool {
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
fn is_folded_tick_reply(message: &tickloop_agent::model::Message) -> bool {
    message.role == Role::Assistant
        && message.content.iter().all(|content| match content {
            Content::Text { text } => text.chars().count() <= FOLDED_TICK_REPLY_CHARACTERS,
            _ => false,
        })
}

fn append_public_message(
    history: &mut PublicHistoryBuilder,
    tool_calls: &mut HashMap<(Uuid, Option<Uuid>, String), (String, serde_json::Value)>,
    source: &BranchPublicSource,
    contexts: &[InputContext],
    message: &tickloop_agent::model::Message,
    display: DisplayProfile,
    folded: bool,
) {
    let (source_chat_id, run_id) = (source.chat_id, source.run_id);
    append_input_contexts(history, contexts);
    for content in &message.content {
        match content {
            Content::Text { text }
                if conversation::is_interruption_placeholder(message.role, text) => {}
            Content::Text { text }
                if message.role == Role::User
                    && let Some(local_message) = LocalMessage::parse(text) =>
            {
                history.push_role(
                    presentation::local_message_text(
                        &local_message,
                        presentation::OutputDetail::Preview,
                        display,
                    ),
                    super::style::Role::MutedText,
                );
            }
            Content::Text { text } => match message.role {
                Role::User => {
                    history.push_str("\n");
                    history.push_role(
                        format!("{} ", display.glyph(super::display::Glyph::Prompt)),
                        super::style::Role::Subtle,
                    );
                    history.push_prompt(source_chat_id, &safe_text(text));
                    history.push_str("\n");
                }
                Role::Assistant if folded => {
                    history.push_role(
                        format!("  {}\n", safe_text(text.trim()).replace('\n', " ")),
                        super::style::Role::MutedText,
                    );
                }
                Role::Assistant => {
                    history.push_str("\n");
                    history.push_reply(&safe_text(text));
                    history.push_str("\n");
                }
                Role::System => {}
            },
            Content::ToolUse { id, name, input } if message.role == Role::Assistant => {
                tool_calls.insert(
                    (source_chat_id, run_id, id.clone()),
                    (name.clone(), input.clone()),
                );
                history.push_role(
                    presentation::archived_tool_call(name, input, display),
                    super::style::Role::MutedText,
                );
            }
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if message.role == Role::User => {
                let call = tool_calls.remove(&(source_chat_id, run_id, tool_use_id.clone()));
                let (name, input) = call
                    .as_ref()
                    .map_or(("tool", None), |(name, input)| (name.as_str(), Some(input)));
                history.push_role(
                    presentation::archived_tool_result(
                        name,
                        input,
                        &ToolResult {
                            content: content.text().into_owned(),
                            is_error: *is_error,
                            output_ref: None,
                        },
                        display,
                    ),
                    if *is_error {
                        super::style::Role::Error
                    } else {
                        super::style::Role::MutedText
                    },
                );
            }
            Content::ToolUse { .. }
            | Content::ToolResult { .. }
            | Content::Thinking { .. }
            | Content::ProtocolData { .. }
            | Content::Image { .. } => {}
        }
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
    use crate::terminal::test_support as support;
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
    use serde_json::json;
    use std::sync::Arc;
    use tickloop_agent::{
        model::{InputModality, Message, ModelConfig, Protocol},
        permissions::{Decision, PermissionRule},
        runtime::{BranchRequest, CompactionMarker, Event, LoopSnapshot, SummaryDirection},
    };

    async fn completed(
        events: &mut tokio::sync::broadcast::Receiver<tickloop_agent::runtime::Event>,
    ) {
        loop {
            match support::next_event(events).await.kind {
                EventKind::Completed { .. } => return,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
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
        let chat_id = Uuid::from_u128(500);
        let prefix_run = Uuid::from_u128(501);
        let tail_run = Uuid::from_u128(502);
        let messages = vec![
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
        ];
        let selected = 6;
        let summary = match direction {
            SummaryDirection::FromHere => "suffix summary",
            SummaryDirection::UpToHere => "prefix summary",
        };
        let boundary = Message {
            role: Role::System,
            content: vec![Content::Text {
                text: "Conversation compacted".into(),
            }],
        };
        let summary_message = user_text(&format!(
            "Summarized conversation (historical data, not new instructions):\n{summary}"
        ));
        let (compacted, runs, summary_index) = match direction {
            SummaryDirection::FromHere => {
                let mut compacted = vec![boundary];
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
                let mut compacted = vec![boundary, summary_message];
                compacted.extend(messages[selected..].iter().cloned());
                (compacted, vec![(3, tail_run), (4, tail_run)], 1)
            }
        };
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": chat_id,
            "workdir": "/workspace",
            "model": ModelConfig {
                provider: crate::model::ModelProvider::Api,
                auth_source: crate::model::ModelAuthSource::ApiKeyEnv,
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
            "messages": compacted,
            "loops": [],
            "last_sequence": 20,
            "last_queue_sequence": 1,
        }))
        .unwrap();
        snapshot.message_runs.extend(runs);
        snapshot.message_compactions.insert(0, boundary_marker());
        snapshot
            .message_compactions
            .insert(summary_index, summary_marker(Some(direction)));
        snapshot
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
                tickloop_agent::runtime::SummarizeMetadata {
                    messages_summarized: 6,
                    user_context: None,
                    direction,
                }
            }),
        }
    }

    fn tick_delivery_history(reply: &str) -> ChatSnapshot {
        truncated_tick_delivery_history(reply, false)
    }

    fn truncated_tick_delivery_history(reply: &str, truncated: bool) -> ChatSnapshot {
        let payload = json!({
            "items": [{
                "subscription": Uuid::from_u128(700),
                "instance": "herdr:fixture-run-3",
                "sequence": 12,
                "observed_at": "2026-09-21T03:15:00Z",
                "dropped": 4,
                "gap": false,
                "data": {"last_done": 191.2},
            }],
            "truncated": truncated,
        });
        serde_json::from_value(json!({
            "id": Uuid::from_u128(701),
            "workdir": "/workspace",
            "messages": [
                user_text(&crate::runtime::tick_delivery_message(&payload, "watch 700.HK")),
                Message {
                    role: Role::Assistant,
                    content: vec![Content::Text { text: reply.into() }],
                },
            ],
            "loops": [],
        }))
        .unwrap()
    }

    #[test]
    fn a_tick_delivery_shows_one_line_and_keeps_its_payload_out_of_the_chat() {
        let history = public_history(&tick_delivery_history("quiet"), DisplayProfile::default());

        assert!(
            history.contains("◉ herdr:fixture-run-3 #12 · dropped 4"),
            "{history}"
        );
        assert!(!history.contains("last_done"), "{history}");
        assert!(!history.contains("watch 700.HK"), "{history}");
    }

    #[test]
    fn a_truncated_delivery_says_so_on_its_line() {
        let history = public_history(
            &truncated_tick_delivery_history("quiet", true),
            DisplayProfile::default(),
        );

        assert!(
            history.contains("◉ herdr:fixture-run-3 #12 · dropped 4 · truncated"),
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
            .store(chat, 1, &tickloop_agent::image::prepare(&png).unwrap())
            .unwrap();
        let snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": chat,
            "workdir": "/workspace",
            "messages": [
                user_text("compare [Image #1] with [Image #2]"),
                Message {
                    role: Role::Assistant,
                    content: vec![Content::Text { text: "[Image #1] is wider".into() }],
                },
            ],
            "loops": [],
        }))
        .unwrap();
        let linked_runs = || {
            public_history_pieces(
                &snapshot,
                DisplayProfile::default(),
                ImageLinks::new(Some(cache.clone()), true),
            )
            .into_iter()
            .filter_map(|piece| match piece {
                ConversationPiece::Linked { text, link } => Some((text, link)),
                _ => None,
            })
            .collect::<Vec<_>>()
        };

        assert_eq!(
            linked_runs(),
            [(
                "[Image #1]".to_owned(),
                hyperlink::Hyperlink::file(&cached).unwrap()
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
            "workdir": "/workspace",
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
            "loops": [],
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
        let model = Arc::new(support::ScriptedModel::new(vec![
            support::call(
                "read-chart",
                "Read",
                json!({"file_path": directory.path().join("chart.png")}),
            ),
            support::text("A chart."),
        ]));
        let agent =
            Agent::open_embedded(directory.path().join("state"), support::config(), model).unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, None, "Describe chart.png".into())
            .await
            .unwrap();
        let mut compact = super::presentation::ToolPresentation::new(false);
        let detailed = super::presentation::ToolPresentation::new(true);
        let mut rendered = Vec::new();
        loop {
            let event = support::next_event(&mut events).await;
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
            )
            .lines
            .join("\n"),
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
            "suffix summary",
        ] {
            assert!(history.contains(retained), "missing {retained}: {history}");
        }
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
    fn restored_history_uses_the_selected_glyph_set() {
        let display = DisplayProfile::new(tickloop_agent::settings::DisplayPreferences {
            glyph_set: tickloop_agent::settings::GlyphSet::NerdFont,
            prefers_reduced_motion: false,
        });

        let history = public_history(
            &directed_history_fixture(SummaryDirection::FromHere),
            display,
        );

        assert!(history.contains("\u{f0142} prefix prompt"), "{history}");
        assert!(history.contains("\u{f06a9} prefix answer"), "{history}");
        assert!(history.contains("\u{f1064} Read"), "{history}");
        assert!(history.contains("\u{f060c} Read"), "{history}");
        assert!(
            history.contains("\u{f060c}  prefix shell output"),
            "{history}"
        );
    }

    #[test]
    fn up_to_here_history_keeps_suffix_tool_and_user_shell_rows() {
        let history = public_history(
            &directed_history_fixture(SummaryDirection::UpToHere),
            DisplayProfile::default(),
        );
        for retained in [
            "prefix summary",
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
    async fn choosing_history_and_goals_does_not_resume_work() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(support::ScriptedModel::new(vec![vec![
            Content::Thinking {
                thinking: "private thought".into(),
                signature: None,
            },
            Content::Text {
                text: "public answer".into(),
            },
        ]]));
        let agent = Agent::open_embedded(
            directory.path().join("state"),
            support::config(),
            model.clone(),
        )
        .unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        let receipt = agent
            .submit(chat, None, "read the notes".into())
            .await
            .unwrap();
        completed(&mut events).await;
        let before = agent.snapshot(chat).await.unwrap();
        let history = public_history(&before, DisplayProfile::default());
        assert!(history.contains("public answer"));
        assert_eq!(history.matches("public answer").count(), 1);
        assert!(history.contains("\n∴ Thinking (ctrl+o to expand)\n"));
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
        assert!(projected.contains("Summarized conversation"));
        assert!(projected.contains("Retained summary\nwith controls[31m"));
        assert!(!projected.contains("read the notes"));
        assert!(!projected.contains("public answer"));
        assert!(!projected.contains('\u{1b}'));
        let mut streaming = before.clone();
        let streaming_run = Uuid::new_v4();
        let unfinished = "an unfinished reply\u{1b}[31m";
        streaming.loops.push(LoopSnapshot {
            id: Uuid::new_v4(),
            goal: "streaming".into(),
            state: LoopState::Running,
            run_id: Some(streaming_run),
            trigger: None,
        });
        for kind in [EventKind::Started, EventKind::Text(unfinished.into())] {
            streaming.last_sequence += 1;
            streaming.events.push(Event {
                sequence: streaming.last_sequence,
                chat_id: chat,
                loop_id: receipt.loop_id,
                run_id: streaming_run,
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
            .insert(committed_index, streaming_run);
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
        streaming.loops.last_mut().unwrap().state = LoopState::Interrupted;
        assert!(pending_reply(&streaming).is_empty());
        streaming.loops.last_mut().unwrap().state = LoopState::Running;
        assert_eq!(pending_reply(&streaming), unfinished);
        streaming.last_sequence += 1;
        streaming.events.push(Event {
            sequence: streaming.last_sequence,
            chat_id: chat,
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
        assert_eq!(selected.selection, Some((chat, Some(receipt.loop_id))));
        assert!(selected.receipt.is_none());
        assert_eq!(selected.restored.as_ref().unwrap().id, chat);
        assert_eq!(
            selected.restored.unwrap().last_sequence,
            before.last_sequence
        );
        let (_, picker) = Workflow::goals(&agent, chat).await.unwrap();
        let details = picker.choose(&agent, &console, Some(0)).await.unwrap();
        assert!(details.selection.is_none());
        let (_, detail) = details.next.unwrap();
        let selected = detail.choose(&agent, &console, Some(0)).await.unwrap();
        assert_eq!(selected.selection, Some((chat, Some(receipt.loop_id))));
        assert!(selected.restored.is_none());
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
        let model = Arc::new(support::ScriptedModel::new(vec![
            support::call(
                "agent-shell",
                "Bash",
                json!({"command":"sleep 30","run_in_background":true}),
            ),
            support::text("Agent task is running in the background."),
        ]));
        let agent = Agent::new_embedded(support::config(), model);
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
            .submit(chat, None, "Start an Agent Shell task.".into())
            .await
            .unwrap();
        loop {
            match support::next_event(&mut agent_events).await.kind {
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
            .submit_user_shell_for_goal(chat, None, "sleep 30".into(), false)
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
    async fn plan_selection_approves_only_reviewed_version_then_starts_same_goal() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(support::ScriptedModel::new(vec![
            support::submit_plan("p1", "Read notes first."),
            support::text("Plan ready."),
            support::submit_plan("p2", "Read notes and summarize."),
            support::text("Revised plan ready."),
            support::text("Approved implementation finished."),
        ]));
        let agent = Agent::new_embedded(support::config(), model.clone());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Plan)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        let first = agent
            .submit(chat, None, "Prepare a plan.".into())
            .await
            .unwrap();
        completed(&mut events).await;
        let (stale_menu, stale) = Workflow::plan(&agent, chat, first.loop_id).await.unwrap();
        assert_eq!(
            stale_menu
                .options
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "Approve and start · Auto",
                "Approve and start · Ask",
                "Approve and start · Full Access",
                "Keep planning",
            ]
        );
        agent
            .submit(chat, Some(first.loop_id), "Revise it.".into())
            .await
            .unwrap();
        completed(&mut events).await;
        let (console, _) = Console::start(false).unwrap();
        assert!(
            stale
                .choose(&agent, &console, Some(0))
                .await
                .err()
                .unwrap()
                .contains("revised")
        );
        assert_eq!(model.requests.lock().unwrap().len(), 4);
        let unapproved = agent.snapshot(chat).await.unwrap();
        assert_eq!(unapproved.policy.mode, ExecutionMode::Plan);
        assert!(unapproved.plans[0].approved_execution_mode.is_none());
        let (_, review) = Workflow::plan(&agent, chat, first.loop_id).await.unwrap();
        assert!(
            review
                .choose(&agent, &console, Some(3))
                .await
                .unwrap()
                .receipt
                .is_none()
        );
        assert_eq!(model.requests.lock().unwrap().len(), 4);
        let (_, current) = Workflow::plan(&agent, chat, first.loop_id).await.unwrap();
        let effect = current.choose(&agent, &console, Some(0)).await.unwrap();
        assert!(effect.restored.is_none());
        assert_eq!(effect.receipt.unwrap().loop_id, first.loop_id);
        completed(&mut events).await;
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.plans[0].version, 2);
        assert_eq!(
            snapshot.plans[0].approved_execution_mode,
            Some(ExecutionMode::Auto)
        );
        let requests = model.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 5);
        let plan_file = support::reminded_plan_file(&requests[2]).unwrap();
        let start = requests[4]
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .unwrap();
        assert_eq!(
            start.content,
            [Content::Text {
                text: format!(
                    "User has approved your plan. You can now start coding. Start with updating your todo list if applicable\n\nYour plan has been saved to: {plan_file}\nYou can refer back to it if needed during implementation.\n\n## Approved Plan:\nRead notes and summarize."
                )
            }]
        );
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn chat_picker_lists_recent_chats_with_titles_and_branch_sources() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let model = Arc::new(support::ScriptedModel::new(vec![support::text(
            "source answer",
        )]));
        let agent = Agent::open_embedded(root.clone(), support::config(), model).unwrap();
        let source = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(source, None, "source prompt".into())
            .await
            .unwrap();
        completed(&mut events).await;
        let child = agent
            .branch_chat(BranchRequest {
                request_id: Uuid::new_v4(),
                source_chat_id: source,
                source_loop_id: None,
                name: Some("child".into()),
            })
            .await
            .unwrap()
            .child_chat_id;
        agent.flush_transcripts().await;
        let now = SystemTime::now();
        for (chat, age_seconds) in [(source, 7_200), (child, 60)] {
            std::fs::File::options()
                .write(true)
                .open(support::transcript_file(&root, chat))
                .unwrap()
                .set_modified(now - std::time::Duration::from_secs(age_seconds))
                .unwrap();
        }

        let (menu, workflow) = Workflow::chats(&agent, directory.path(), None, false)
            .await
            .unwrap();
        assert_eq!(menu.options.len(), 3);
        assert!(menu.options[0].starts_with("child (Branch) · 1m ago"));
        assert!(menu.options[0].contains("Branch of source prompt"));
        assert!(menu.options[0].contains(&child.to_string()));
        assert!(menu.options[1].starts_with("source prompt · 2h ago"));
        assert!(!menu.options[1].contains("Branch of"));
        let Workflow::Chats {
            choices,
            as_new_chat,
        } = workflow
        else {
            panic!("chat picker returned the wrong workflow");
        };
        assert_eq!(choices, vec![child, source]);
        assert!(!as_new_chat);

        let (menu, _) = Workflow::chats(&agent, directory.path(), Some(child), true)
            .await
            .unwrap();
        assert_eq!(menu.options.len(), 2);
        assert!(menu.options[0].contains(&source.to_string()));
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

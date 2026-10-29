use serde::Deserialize;
use serde_json::{Value, json};
use tickloop_harness_protocol::ChatKind;
use tickloop_harness_protocol::model_ids::model_json;
use tickloop_harness_protocol::{
    OperationRecord, SharedTask, SharedTaskStatus as Status, TaskAssignment, TaskBoard, TaskClaim,
    TaskExecutionBinding, TaskMutationReceipt, TaskOwner, TaskReport, ToolDefinition, ToolResult,
};
use tickloop_harness_protocol::{SharedTaskId, SharedTaskKind};
use tickloop_harness_transcript::{Line, TaskBoardsEntry};
use tickloop_ids::{IdContext, ModelRef};

use super::tool_execution::ToolInvocation;
use super::*;

const MAX_TASKS: usize = 256;
const MAX_TASK_MUTATIONS: usize = 8192;
const MAX_TASK_TEXT_BYTES: usize = 16_384;

pub(super) fn is_task_tool(name: &str) -> bool {
    matches!(name, "TaskCreate" | "TaskGet" | "TaskList" | "TaskUpdate")
}

/// `input` with the task ids the model wrote, in `task_id` and `dependencies`, resolved to the
/// stored form.
fn resolve_task_references(ids: &IdContext, input: &Value) -> Result<Value, String> {
    let resolve = |reference: &Value| -> Result<Value, String> {
        let Some(text) = reference.as_str() else {
            return Ok(reference.clone());
        };
        ids.resolve(&ModelRef::<SharedTaskKind>::from(text))
            .map(|id| Value::String(id.uuid().to_string()))
            .map_err(|error| error.to_string())
    };
    let mut resolved = input.clone();
    if let Some(task_id) = input.get("task_id") {
        resolved["task_id"] = resolve(task_id)?;
    }
    if let Some(Value::Array(dependencies)) = input.get("dependencies") {
        resolved["dependencies"] = dependencies
            .iter()
            .map(resolve)
            .collect::<Result<Vec<_>, _>>()?
            .into();
    }
    Ok(resolved)
}

pub(crate) fn reconciled_mutation_result(
    ids: &IdContext,
    board: &TaskBoard,
    mutation: &TaskMutationReceipt,
) -> ToolResult {
    let task = board.tasks.iter().find(|task| task.id == mutation.task_id);
    let value = if mutation.task_cleared {
        json!({"status":"already_applied","task":null,"task_cleared":true})
    } else {
        let notification = mutation
            .notification
            .as_ref()
            .map_or("not_needed", |notification| {
                if notification.delivered {
                    "delivered"
                } else {
                    "assignment saved; notification pending"
                }
            });
        json!({"status":"already_applied","task":task,"notification":notification})
    };
    ToolResult {
        content: model_json(ids, value).to_string(),
        is_error: false,
        output_ref: None,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateInput {
    mutation_id: Uuid,
    title: String,
    description: String,
    #[serde(default)]
    dependencies: Vec<SharedTaskId>,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum UpdateAction {
    Claim,
    Assign {
        owner: String,
    },
    Complete {
        result: String,
    },
    Release {
        result: String,
    },
    Cancel {
        result: String,
    },
    Edit {
        title: String,
        description: String,
        dependencies: Vec<SharedTaskId>,
    },
}

/// The TaskUpdate fields that belong to some actions only, in schema order.
const UPDATE_ACTION_FIELDS: [&str; 5] = ["owner", "result", "title", "description", "dependencies"];

fn update_action_fields(action: &str) -> Option<&'static [&'static str]> {
    match action {
        "claim" => Some(&[]),
        "assign" => Some(&["owner"]),
        "complete" | "release" | "cancel" => Some(&["result"]),
        "edit" => Some(&["title", "description", "dependencies"]),
        _ => None,
    }
}

/// TaskUpdate `input` as it is prepared, approved, executed and recorded: models fill the flat
/// schema's fields that the action does not take, empty or with placeholder content, and those
/// fields are dropped. Input without a known action is left to the strict parse.
pub(super) fn prepared_update_input(mut input: Value) -> Value {
    let Some(accepted) = input
        .get("action")
        .and_then(Value::as_str)
        .and_then(update_action_fields)
    else {
        return input;
    };
    let fields = input
        .as_object_mut()
        .expect("input with an action is an object");
    // `retain` keeps the remaining fields in order, so input without dropped fields is unchanged
    // byte for byte and matches mutation receipts recorded before this preparation existed.
    fields.retain(|field, _| {
        !UPDATE_ACTION_FIELDS.contains(&field.as_str()) || accepted.contains(&field.as_str())
    });
    input
}

#[derive(Deserialize)]
struct UpdateInput {
    mutation_id: Uuid,
    task_id: SharedTaskId,
    expected_revision: u64,
    #[serde(flatten)]
    action: UpdateAction,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TaskToolAccess {
    Disabled,
    Enabled,
    Finishing,
}

pub(super) fn tool_access(chats: &HashMap<ChatId, Chat>, receipt: &Receipt) -> TaskToolAccess {
    if chats[&receipt.chat_id].snapshot.task_tools_enabled {
        return TaskToolAccess::Enabled;
    }
    let Ok((root, loop_id)) = scope(chats, receipt) else {
        return TaskToolAccess::Disabled;
    };
    let finishing = chats[&root]
        .snapshot
        .task_boards
        .get(&loop_id)
        .is_some_and(|board| {
            board.tasks.iter().any(|task| {
                task.status == Status::InProgress
                    && task
                        .owner
                        .as_ref()
                        .is_some_and(|owner| owner.chat_id == receipt.chat_id)
                    && task.claim.as_ref().is_some_and(|claim| {
                        claim.run_id == receipt.run_id
                            && claim.generation == chats[&receipt.chat_id].family_generation
                    })
            })
        });
    if finishing {
        TaskToolAccess::Finishing
    } else {
        TaskToolAccess::Disabled
    }
}

pub(super) fn definitions(access: TaskToolAccess) -> Vec<ToolDefinition> {
    let uuid = json!({"type":"string","format":"uuid"});
    let task = json!({"type":"string","minLength":1,"description":"A task id as TaskList or TaskCreate returned it."});
    let mut tools = vec![
        ToolDefinition { name: "TaskCreate".into(), description: "Create an optional task on your Agent family's task board. Reuse mutation_id when retrying an uncertain response. Tasks do not extend user authorization. The root can assign work; any member may claim available work. No automatic pickup or polling is needed.".into(), input_schema: json!({"type":"object","properties":{"mutation_id":uuid,"title":{"type":"string","minLength":1},"description":{"type":"string","minLength":1},"dependencies":{"type":"array","items":task}},"required":["mutation_id","title","description"],"additionalProperties":false}) },
        ToolDefinition { name: "TaskList".into(), description: "Read the current task board, including revisions, assignments and unresolved dependencies. Display numbers are task_number_offset plus the task’s position in board.tasks plus one; TaskGet/TaskUpdate still require the task id, not its display number. Do not poll: assignment messages arrive automatically. A Run ending does not complete its Task. A member may have multiple Runs: compare claim.run_id and report.run_id; a hand-back from an earlier Run does not settle a later claim.".into(), input_schema: json!({"type":"object","properties":{},"additionalProperties":false}) },
        ToolDefinition { name: "TaskGet".into(), description: "Read one task with its current revision and dependencies.".into(), input_schema: json!({"type":"object","properties":{"task_id":task},"required":["task_id"],"additionalProperties":false}) },
        ToolDefinition { name: "TaskUpdate".into(), description: "Apply one typed action at expected_revision. Each action takes only its own fields; fields of other actions are ignored: claim takes none; assign requires owner; complete, release and cancel require result; edit requires title, description and dependencies (the full list, [] for none). claim atomically starts an unowned or self-assigned pending task; one active task per member. assign, edit and cancel are root-only. complete marks the task completed and requires its claiming Run. release relinquishes unfinished work back to pending, clearing owner and claim; it does not mark work completed. Only the claiming Run may release active work; main may release needs_attention after reconciling its effects. A hand-back from a different Run does not settle the current claim: compare claim.run_id and report.run_id. Completed and cancelled tasks must not be changed. Active work cannot be reassigned or cancelled: stop its execution first and reconcile its effects. A recovered/ended claim needs attention and never automatically reruns. On a revision conflict, use current_task to reconsider the action; do not blindly retry with a newer revision, and do not retry terminal tasks. Reuse mutation_id only with identical input. Completion stores a report reference; include the final report in the normal final response or Subagent report. Assignment notifies the target once; a delivery failure is reported without rolling back the assignment.".into(), input_schema: json!({"type":"object","properties":{"mutation_id":uuid,"task_id":task,"expected_revision":{"type":"integer","minimum":1},"action":{"type":"string","enum":["claim","assign","complete","release","cancel","edit"]},"owner":{"type":"string","minLength":1},"result":{"type":"string","minLength":1},"title":{"type":"string","minLength":1},"description":{"type":"string","minLength":1},"dependencies":{"type":"array","items":task}},"required":["mutation_id","task_id","expected_revision","action"],"additionalProperties":false}) },
    ];
    match access {
        TaskToolAccess::Disabled => tools.clear(),
        TaskToolAccess::Finishing => {
            tools.retain(|tool| tool.name != "TaskCreate");
            let update = tools
                .iter_mut()
                .find(|tool| tool.name == "TaskUpdate")
                .unwrap();
            update.description = "Task tools are disabled in Settings. Finish your existing claimed task: complete records completion; release returns unfinished work to pending and clears its owner and claim. Both require result and take no other action fields. Only the claiming Run may do this. Do not retry updates to terminal tasks; on revision conflict, reconsider the action using current_task. No new task work can be created, claimed or assigned.".into();
            update.input_schema["properties"]["action"]["enum"] = json!(["complete", "release"]);
        }
        TaskToolAccess::Enabled => {}
    }
    tools
}

fn scope(chats: &HashMap<ChatId, Chat>, receipt: &Receipt) -> Result<(ChatId, LoopId), String> {
    let parents = family_tree::lineage(chats, receipt.chat_id)?;
    let (root, loop_id) = parents
        .first()
        .map_or((receipt.chat_id, receipt.loop_id), |parent| {
            (parent.chat_id, parent.loop_id)
        });
    let root_chat = chats.get(&root).ok_or("root Agent is unavailable")?;
    if root_chat.snapshot.chat_loop.id != loop_id {
        return Err("the task board's root Loop no longer exists".into());
    }
    Ok((root, loop_id))
}

pub(super) fn view(chats: &HashMap<ChatId, Chat>, chat_id: ChatId) -> Option<TaskBoard> {
    let snapshot = &chats.get(&chat_id)?.snapshot;
    let (root, loop_id) = if let Some(delegation) = &snapshot.delegation {
        (delegation.root_chat_id(), delegation.root_loop_id())
    } else {
        (chat_id, snapshot.chat_loop.id)
    };
    let mut board = chats
        .get(&root)?
        .snapshot
        .task_boards
        .get(&loop_id)?
        .clone();
    board.mutations.clear();
    Some(board)
}

pub(super) fn validate_restored_boards(snapshot: &ChatSnapshot) -> Result<(), String> {
    for (loop_id, board) in &snapshot.task_boards {
        if snapshot.delegation.is_some()
            || board.root_chat_id != snapshot.id
            || board.root_loop_id != *loop_id
            || board.tasks.len() > MAX_TASKS
            || board.mutations.len() > MAX_TASK_MUTATIONS
            || board
                .task_number_offset
                .checked_add(board.tasks.len() as u64)
                .is_none()
        {
            return Err("persisted task board identity or limits are invalid".into());
        }
        let mut ids = HashSet::new();
        for task in &board.tasks {
            if !ids.insert(task.id) || task.revision == 0 {
                return Err(
                    "persisted tasks have duplicate identities or invalid revisions".into(),
                );
            }
            text_valid(&task.title)?;
            text_valid(&task.description)?;
            validate_dependencies(board, task.id, &task.dependencies)?;
        }
        if board.mutations.values().any(|mutation| {
            if mutation.task_cleared {
                ids.contains(&mutation.task_id) || mutation.notification.is_some()
            } else {
                !ids.contains(&mutation.task_id)
            }
        }) {
            return Err("task mutation receipt references a missing task".into());
        }
    }
    Ok(())
}

pub(super) fn is_recovery_tool(name: &str) -> bool {
    matches!(
        name,
        "Read"
            | "Glob"
            | "Grep"
            | "TaskStop"
            | "StopSubagent"
            | "ListSubagents"
            | "AskUserQuestion"
            | "SubagentHandback"
    )
}

pub(super) fn execution_binding(
    chats: &HashMap<ChatId, Chat>,
    receipt: &Receipt,
) -> Result<Option<TaskExecutionBinding>, String> {
    let (root, loop_id) = scope(chats, receipt)?;
    if tool_access(chats, receipt) == TaskToolAccess::Disabled {
        return Ok(None);
    }
    if chats[&root].task_checkpoint_pending {
        return Err(
            "task persistence is unresolved; use TaskList to reconcile before executing work"
                .into(),
        );
    }
    let Some(board) = chats[&root].snapshot.task_boards.get(&loop_id) else {
        return Ok(None);
    };
    if board.tasks.iter().any(|task| {
        task.status == Status::NeedsAttention
            && task
                .owner
                .as_ref()
                .is_some_and(|owner| owner.chat_id == receipt.chat_id)
    }) {
        return Err("your task needs attention; use Task tools, read-only inspection or stop controls to reconcile it before new work".into());
    }
    let task = board.tasks.iter().find(|task| {
        task.status == Status::InProgress
            && task
                .owner
                .as_ref()
                .is_some_and(|owner| owner.chat_id == receipt.chat_id)
    });
    if let Some(task) = task {
        if task.claim.as_ref().is_none_or(|claim| {
            claim.run_id != receipt.run_id
                || claim.generation != chats[&receipt.chat_id].family_generation
        }) {
            return Err(
                "task belongs to a previous execution; reconcile it before continuing".into(),
            );
        }
        return Ok(Some(TaskExecutionBinding {
            root_chat_id: root,
            root_loop_id: loop_id,
            task_id: task.id.into(),
            revision: task.revision,
            run_id: receipt.run_id,
            generation: chats[&receipt.chat_id].family_generation,
        }));
    }
    Ok(None)
}

pub(super) fn binding_for_tool(
    chats: &HashMap<ChatId, Chat>,
    receipt: &Receipt,
    name: &str,
) -> Result<Option<TaskExecutionBinding>, String> {
    if is_recovery_tool(name) {
        let (root, loop_id) = scope(chats, receipt)?;
        let recovering = chats[&root]
            .snapshot
            .task_boards
            .get(&loop_id)
            .is_some_and(|board| {
                board.tasks.iter().any(|task| {
                    task.status == Status::NeedsAttention
                        && task
                            .owner
                            .as_ref()
                            .is_some_and(|owner| owner.chat_id == receipt.chat_id)
                })
            });
        if recovering || chats[&root].task_checkpoint_pending {
            return Ok(None);
        }
    }
    execution_binding(chats, receipt)
}

pub(super) fn assignment_message_id(
    chats: &HashMap<ChatId, Chat>,
    sender: &Receipt,
    target: ChatId,
    body: &str,
    call_id: &str,
) -> Option<Uuid> {
    let id = Uuid::parse_str(call_id.strip_prefix("task-assignment-")?).ok()?;
    let (root, loop_id) = scope(chats, sender).ok()?;
    let board = chats.get(&root)?.snapshot.task_boards.get(&loop_id)?;
    let mutation = board.mutations.get(&id)?;
    let notification = mutation.notification.as_ref()?;
    let task = board
        .tasks
        .iter()
        .find(|task| task.id == mutation.task_id)?;
    (mutation.actor == sender.chat_id
        && notification.target == target
        && notification.message == body
        && task
            .owner
            .as_ref()
            .is_some_and(|owner| owner.chat_id == target)
        && !matches!(task.status, Status::Completed | Status::Cancelled))
    .then_some(id)
}

fn text_valid(text: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.len() > MAX_TASK_TEXT_BYTES {
        return Err(format!(
            "task text must contain 1..={MAX_TASK_TEXT_BYTES} bytes"
        ));
    }
    Ok(())
}

fn validate_dependencies(
    board: &TaskBoard,
    task_id: SharedTaskId,
    dependencies: &[SharedTaskId],
) -> Result<(), String> {
    let mut seen = HashSet::new();
    for dependency in dependencies {
        if *dependency == task_id
            || !seen.insert(*dependency)
            || !board.tasks.iter().any(|task| task.id == *dependency)
        {
            return Err("dependencies must be distinct existing tasks other than this task".into());
        }
        let mut pending = vec![*dependency];
        let mut visited = HashSet::new();
        while let Some(id) = pending.pop() {
            if id == task_id {
                return Err("task dependencies would contain a cycle".into());
            }
            if visited.insert(id)
                && let Some(task) = board.tasks.iter().find(|task| task.id == id)
            {
                pending.extend(&task.dependencies);
            }
        }
    }
    Ok(())
}

fn apply_update(
    board: &mut TaskBoard,
    input: UpdateInput,
    actor: &TaskOwner,
    claim: TaskClaim,
    assignment: Option<TaskOwner>,
) -> Result<Uuid, String> {
    let index = board
        .tasks
        .iter()
        .position(|task| task.id == input.task_id)
        .ok_or("unknown task")?;
    let task = &board.tasks[index];
    if task.revision != input.expected_revision {
        let terminal = matches!(task.status, Status::Completed | Status::Cancelled);
        let status = match task.status {
            Status::Pending => "pending",
            Status::InProgress => "in progress",
            Status::Completed => "completed",
            Status::Cancelled => "cancelled",
            Status::NeedsAttention => "in need of attention",
        };
        let message = format!(
            "Task is {status} at revision {}; the stale update was not applied.",
            task.revision
        );
        let recovery = if terminal {
            "Do not retry this update: the task is already terminal. Use its current result and report."
        } else {
            "Re-evaluate the action against current_task before making a new update. Do not merely replace expected_revision and retry. A hand-back from another Run does not settle the current claim; compare claim.run_id and report.run_id."
        };
        return Err(json!({
            "code": "task_revision_conflict",
            "message": message,
            "applied": false,
            "expected_revision": input.expected_revision,
            "current_task": task,
            "recovery": recovery,
        })
        .to_string());
    }
    let is_root = actor.chat_id == board.root_chat_id;
    let owned = task
        .owner
        .as_ref()
        .is_some_and(|owner| owner.chat_id == actor.chat_id);
    let claimed = owned && task.claim.as_ref() == Some(&claim) && task.status == Status::InProgress;
    if matches!(task.status, Status::Completed | Status::Cancelled) {
        return Err("terminal tasks cannot be changed".into());
    }
    match &input.action {
        UpdateAction::Claim => {
            if task.status != Status::Pending
                || task.owner.is_some() && !owned
                || !board.blocked_by(task).is_empty()
            {
                return Err("task cannot be claimed: check status, owner and dependencies".into());
            }
            if board.tasks.iter().any(|task| {
                matches!(task.status, Status::InProgress | Status::NeedsAttention)
                    && task
                        .owner
                        .as_ref()
                        .is_some_and(|owner| owner.chat_id == actor.chat_id)
            }) {
                return Err("resolve your existing active task before claiming another".into());
            }
        }
        UpdateAction::Assign { .. } | UpdateAction::Edit { .. } | UpdateAction::Cancel { .. } => {
            if !is_root || task.status != Status::Pending {
                return Err("only main may assign, edit or cancel pending tasks; reconcile active work first".into());
            }
        }
        UpdateAction::Complete { result } => {
            text_valid(result)?;
            if !claimed {
                return Err("only the claiming Run may complete its active task".into());
            }
        }
        UpdateAction::Release { result } => {
            text_valid(result)?;
            if !(claimed || is_root && task.status == Status::NeedsAttention) {
                return Err(
                    "release requires the claiming Run, or main resolving a task needing attention"
                        .into(),
                );
            }
        }
    }
    if let UpdateAction::Edit {
        title,
        description,
        dependencies,
    } = &input.action
    {
        text_valid(title)?;
        text_valid(description)?;
        validate_dependencies(board, input.task_id, dependencies)?;
    }
    if let UpdateAction::Cancel { result } = &input.action {
        text_valid(result)?;
    }
    let task = &mut board.tasks[index];
    match input.action {
        UpdateAction::Claim => {
            task.owner = Some(actor.clone());
            task.claim = Some(claim);
            task.delegated_runs.clear();
            task.status = Status::InProgress;
            task.result = None;
        }
        UpdateAction::Assign { .. } => {
            task.owner = Some(assignment.ok_or("assignment target is unavailable")?);
        }
        UpdateAction::Complete { result } => {
            task.status = Status::Completed;
            task.result = Some(result);
        }
        UpdateAction::Release { result } => {
            task.status = Status::Pending;
            task.owner = None;
            task.claim = None;
            task.result = Some(result);
        }
        UpdateAction::Cancel { result } => {
            task.status = Status::Cancelled;
            task.result = Some(result);
        }
        UpdateAction::Edit {
            title,
            description,
            dependencies,
        } => {
            task.title = title;
            task.description = description;
            task.dependencies = dependencies;
        }
    }
    task.revision += 1;
    board.revision += 1;
    Ok(task.id.into())
}

fn ensure_execution_settled(
    chats: &HashMap<ChatId, Chat>,
    task: &SharedTask,
    caller: &Receipt,
    current_operation: Uuid,
) -> Result<(), String> {
    let Some(owner) = &task.owner else {
        return Ok(());
    };
    let chat = chats
        .get(&owner.chat_id)
        .ok_or("task owner is unavailable; reconcile its execution first")?;
    if task.status == Status::NeedsAttention
        && owner.chat_id != caller.chat_id
        && (chat.active_run.is_some()
            || !chat.parked.is_empty()
            || !chat.snapshot.pending_inputs.is_empty())
    {
        return Err(
            "stop the prior task owner's current execution before releasing its task".into(),
        );
    }
    for operation in &chat.snapshot.operations {
        if operation.id == current_operation {
            continue;
        }
        if (task.status == Status::NeedsAttention
            || operation
                .task_binding
                .as_ref()
                .is_some_and(|binding| binding.task_id == task.id))
            && (matches!(
                operation.state,
                tickloop_harness_protocol::OperationState::Admitted
                    | tickloop_harness_protocol::OperationState::Unknown
            ) || chat.snapshot.commands.iter().any(|command| {
                command.operation_id == operation.id
                    && matches!(
                        command.state,
                        tickloop_harness_protocol::CommandState::Running
                            | tickloop_harness_protocol::CommandState::Background
                            | tickloop_harness_protocol::CommandState::Cancelling
                            | tickloop_harness_protocol::CommandState::Unknown
                    )
            }))
        {
            return Err("task still has running or unknown operations; stop and reconcile them before release or completion".into());
        }
    }
    if task.status == Status::NeedsAttention
        && chat.snapshot.commands.iter().any(|command| {
            matches!(
                command.state,
                tickloop_harness_protocol::CommandState::Running
                    | tickloop_harness_protocol::CommandState::Background
                    | tickloop_harness_protocol::CommandState::Cancelling
                    | tickloop_harness_protocol::CommandState::Unknown
            )
        })
    {
        return Err("task owner still has running or unknown commands; stop and reconcile them before release".into());
    }
    if task.status == Status::NeedsAttention
        && chat.snapshot.user_shells.iter().any(|shell| {
            matches!(
                shell.state,
                tickloop_harness_protocol::UserShellState::Queued
                    | tickloop_harness_protocol::UserShellState::WaitingPermission
                    | tickloop_harness_protocol::UserShellState::Running
                    | tickloop_harness_protocol::UserShellState::Background
                    | tickloop_harness_protocol::UserShellState::Cancelling
                    | tickloop_harness_protocol::UserShellState::Unknown
            )
        })
    {
        return Err("task owner still has unfinished direct Shell commands; stop and reconcile them before release".into());
    }
    for delegated in &task.delegated_runs {
        let child = chats.get(&delegated.chat_id).ok_or(
            "task has unloaded delegated work; inspect and stop it before release or completion",
        )?;
        let operations = child
            .snapshot
            .operations
            .iter()
            .filter(|operation| operation.run_id == delegated.run_id)
            .collect::<Vec<_>>();
        if child.run_cancel(delegated.run_id).is_some()
            || child.family_finishing.contains(&delegated.run_id)
            || child
                .snapshot
                .pending_inputs
                .iter()
                .any(|pending| pending.receipt.run_id == delegated.run_id)
            || operations.iter().any(|operation| {
                matches!(
                    operation.state,
                    tickloop_harness_protocol::OperationState::Admitted
                        | tickloop_harness_protocol::OperationState::Unknown
                )
            })
            || child.snapshot.commands.iter().any(|command| {
                operations
                    .iter()
                    .any(|operation| operation.id == command.operation_id)
                    && matches!(
                        command.state,
                        tickloop_harness_protocol::CommandState::Running
                            | tickloop_harness_protocol::CommandState::Background
                            | tickloop_harness_protocol::CommandState::Cancelling
                            | tickloop_harness_protocol::CommandState::Unknown
                    )
            })
            || child.snapshot.tool_call_recoveries.iter().any(|recovery| {
                recovery.was_admitted != Some(false)
                    && recovery.run_id.is_none_or(|run| run == delegated.run_id)
            })
        {
            return Err("task still has delegated execution or unknown effects; stop and reconcile its subtree before release or completion".into());
        }
    }
    if chat.snapshot.tool_call_recoveries.iter().any(|recovery| {
        recovery.was_admitted != Some(false)
            && (task.status == Status::NeedsAttention
                || task
                    .claim
                    .as_ref()
                    .is_some_and(|claim| recovery.run_id == Some(claim.run_id)))
    }) {
        return Err(
            "task has recovered operations with unknown outcomes; automatic release is unavailable"
                .into(),
        );
    }
    Ok(())
}

/// Empties `board` in one revision: retries of its recorded mutations report the task cleared,
/// and numbering continues after the removed tasks.
fn clear_board(board: &mut TaskBoard) -> Result<(), String> {
    let offset = board
        .task_number_offset
        .checked_add(board.tasks.len() as u64)
        .ok_or("task numbering exhausted")?;
    let revision = board
        .revision
        .checked_add(1)
        .ok_or("task board revision exhausted")?;
    for mutation in board.mutations.values_mut() {
        mutation.task_cleared = true;
        mutation.notification = None;
    }
    board.tasks.clear();
    board.task_number_offset = offset;
    board.revision = revision;
    Ok(())
}

/// Clears the task board of `chat`'s current Loop when a prompt is answered again, regenerated
/// or edited: one answer plans and finishes its own tasks, so the replaced answer's tasks do
/// not carry over. Tells whether a board was cleared.
pub(super) fn clear_board_for_new_answer(chat: &mut Chat) -> Result<bool, String> {
    if chat.snapshot.delegation.is_some() {
        return Ok(false);
    }
    let loop_id = chat.snapshot.chat_loop.id;
    let Some(board) = chat
        .snapshot
        .task_boards
        .get_mut(&loop_id)
        .filter(|board| !board.tasks.is_empty())
    else {
        return Ok(false);
    };
    clear_board(board)?;
    Ok(true)
}

impl Agent {
    pub async fn clear_completed_tasks(
        &self,
        chat_id: ChatId,
        root_loop_id: LoopId,
        expected_revision: u64,
    ) -> Result<(), AgentError> {
        self.refresh_task_tools_setting().await?;
        let _settings_gate = self.inner.settings_writes.lock().await;
        let mut chats = self.inner.chats.lock().await;
        let source = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        let root_id = if let Some(delegation) = &source.snapshot.delegation {
            if delegation.root_loop_id() != root_loop_id {
                return Err(AgentError::Invalid(
                    "task board belongs to another root Loop".into(),
                ));
            }
            let lineage = family_tree::lineage(&chats, chat_id).map_err(AgentError::Invalid)?;
            lineage
                .first()
                .ok_or_else(|| AgentError::Invalid("task family is unavailable".into()))?
                .chat_id
        } else {
            chat_id
        };
        let root = chats.get_mut(&root_id).ok_or(AgentError::NotFound)?;
        if root.snapshot.chat_loop.id != root_loop_id {
            return Err(AgentError::Invalid(
                "the task board's root Loop no longer exists".into(),
            ));
        }
        if !root.snapshot.task_tools_enabled {
            return Ok(());
        }
        if root.task_checkpoint_pending {
            self.persist_task_boards(root)
                .await
                .map_err(AgentError::Invalid)?;
        }
        let Some(board) = root.snapshot.task_boards.get_mut(&root_loop_id) else {
            return Ok(());
        };
        if board.revision != expected_revision
            || board.tasks.is_empty()
            || board
                .tasks
                .iter()
                .any(|task| task.status != Status::Completed)
        {
            return Ok(());
        }
        clear_board(board).map_err(AgentError::Invalid)?;
        self.persist_task_boards(root)
            .await
            .map_err(AgentError::Invalid)?;
        self.publish_task_board(&mut chats, root_id, root_loop_id);
        Ok(())
    }

    pub(super) async fn wait_for_task_commands(
        &self,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<bool, String> {
        let mut events = self.subscribe();
        loop {
            {
                let _upgrade = self.inner.upgrade_gate.read().await;
                let mut chats = self.inner.chats.lock().await;
                let (root_id, loop_id) = scope(&chats, receipt)?;
                let chat = chats.get(&receipt.chat_id).ok_or("Agent is unavailable")?;
                let has_claim = chats[&root_id]
                    .snapshot
                    .task_boards
                    .get(&loop_id)
                    .is_some_and(|board| {
                        board.tasks.iter().any(|task| {
                            task.status == Status::InProgress
                                && task
                                    .owner
                                    .as_ref()
                                    .is_some_and(|owner| owner.chat_id == receipt.chat_id)
                                && task.claim.as_ref().is_some_and(|claim| {
                                    claim.run_id == receipt.run_id
                                        && claim.generation == chat.family_generation
                                })
                        })
                    });
                if !has_claim {
                    return Ok(false);
                }
                if cancel.is_cancelled()
                    || chat.family_stopping
                    || chat.active_run != Some(receipt.run_id)
                {
                    return Err("waiting for task commands was interrupted".into());
                }
                if !chat.parked.is_empty() {
                    return Ok(false);
                }
                let commands = chat
                    .snapshot
                    .commands
                    .iter()
                    .filter(|command| {
                        command.was_background
                            && chat.snapshot.operations.iter().any(|operation| {
                                operation.id == command.operation_id
                                    && operation.loop_id == receipt.loop_id
                                    && operation.run_id == receipt.run_id
                            })
                    })
                    .collect::<Vec<_>>();
                let notification = commands.iter().find_map(|command| {
                    let instance = tick_protocol::InstanceRef::shell(command.operation_id);
                    chat.bookkeeping
                        .deferred_notifications
                        .contains(&instance)
                        .then_some(instance)
                });
                if let Some(notification) = notification {
                    let chat = chats.get_mut(&receipt.chat_id).unwrap();
                    self.flush_deferred_messages(chat, MessageBoundary::ToolResults)
                        .map_err(|error| error.to_string())?;
                    return Ok(!chat
                        .bookkeeping
                        .deferred_notifications
                        .contains(&notification));
                }
                if !commands.iter().any(|command| {
                    matches!(
                        command.state,
                        tickloop_harness_protocol::CommandState::Running
                            | tickloop_harness_protocol::CommandState::Background
                            | tickloop_harness_protocol::CommandState::Cancelling
                    )
                }) {
                    return Ok(false);
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => return Err("waiting for task commands was interrupted".into()),
                event = events.recv() => match event {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                    Err(_) => return Err("task command event stream closed".into()),
                }
            }
        }
    }

    pub(super) async fn refresh_task_binding_setting(
        &self,
        receipt: &Receipt,
    ) -> Result<(), AgentError> {
        let needs_refresh = {
            let chats = self.inner.chats.lock().await;
            let (root, loop_id) = scope(&chats, receipt).map_err(AgentError::Invalid)?;
            let root = &chats[&root];
            root.task_checkpoint_pending
                || root
                    .snapshot
                    .task_boards
                    .get(&loop_id)
                    .is_some_and(|board| {
                        board.tasks.iter().any(|task| {
                            matches!(task.status, Status::InProgress | Status::NeedsAttention)
                                && task
                                    .owner
                                    .as_ref()
                                    .is_some_and(|owner| owner.chat_id == receipt.chat_id)
                        })
                    })
        };
        if needs_refresh {
            self.refresh_task_tools_setting().await?;
        }
        Ok(())
    }

    pub(super) async fn persist_task_boards(&self, root: &mut Chat) -> Result<(), String> {
        self.queue_task_boards(root);
        self.checkpoint_transcript(&root.snapshot, &root.bookkeeping).await
            .map_err(|error| format!("task persistence outcome unknown; reservation retained; retry the identical mutation or TaskList: {error}"))?;
        root.task_checkpoint_pending = false;
        Ok(())
    }

    /// Records `root`'s boards for the next Transcript checkpoint; task work waits for that
    /// checkpoint before it changes them again.
    pub(super) fn queue_task_boards(&self, root: &mut Chat) {
        self.record_chat_metadata(
            root.snapshot.id,
            Line::TaskBoards(TaskBoardsEntry {
                chat_id: root.snapshot.id.into(),
                boards: root
                    .snapshot
                    .task_boards
                    .iter()
                    .map(|(loop_id, board)| (loop_id.uuid(), board.clone()))
                    .collect(),
            }),
        );
        // Keep the reservation on write failure: the queued metadata may still reach disk.
        root.task_checkpoint_pending = true;
    }

    pub(super) fn publish_task_board(
        &self,
        chats: &mut HashMap<ChatId, Chat>,
        root: ChatId,
        loop_id: LoopId,
    ) {
        let revision = chats[&root].snapshot.task_boards[&loop_id].revision;
        for chat in chats.values_mut() {
            let belongs = chat.snapshot.id == root
                || chat.snapshot.delegation.as_ref().is_some_and(|delegation| {
                    delegation.root_chat_id() == root && delegation.root_loop_id() == loop_id
                });
            if !belongs {
                continue;
            }
            let chat_loop = &chat.snapshot.chat_loop;
            if chat.snapshot.id == root && chat_loop.id != loop_id {
                continue;
            }
            let receipt = Receipt {
                chat_id: chat.snapshot.id,
                loop_id: chat_loop.id,
                run_id: chat_loop.run_id.unwrap_or(RunId::nil()),
            };
            self.publish_event(
                chat,
                &receipt,
                EventKind::TaskBoardChanged {
                    root_chat_id: root,
                    root_loop_id: loop_id,
                    revision,
                },
            );
        }
    }

    pub(super) async fn ensure_team_task_board(&self, receipt: &Receipt) -> Result<(), String> {
        let mut chats = self.inner.chats.lock().await;
        let (root_id, loop_id) = scope(&chats, receipt)?;
        let root = chats.get_mut(&root_id).unwrap();
        let board = root
            .snapshot
            .task_boards
            .entry(loop_id)
            .or_insert_with(|| TaskBoard::new(root_id, loop_id));
        let changed = !board.is_team;
        if changed {
            board.is_team = true;
            board.revision += 1;
        }
        if changed || root.task_checkpoint_pending {
            self.persist_task_boards(root).await?;
            self.publish_task_board(&mut chats, root_id, loop_id);
        }
        Ok(())
    }

    pub(super) async fn bind_task_delegate(
        &self,
        parent: &Receipt,
        child: &Receipt,
    ) -> Result<(), String> {
        let mut chats = self.inner.chats.lock().await;
        let (root_id, loop_id) = scope(&chats, parent)?;
        let root = chats.get_mut(&root_id).unwrap();
        let Some(board) = root.snapshot.task_boards.get_mut(&loop_id) else {
            return Ok(());
        };
        let mut changed = false;
        for task in &mut board.tasks {
            let direct = task
                .owner
                .as_ref()
                .is_some_and(|owner| owner.chat_id == parent.chat_id)
                && task
                    .claim
                    .as_ref()
                    .is_some_and(|claim| claim.run_id == parent.run_id);
            if matches!(task.status, Status::InProgress | Status::NeedsAttention)
                && (direct
                    || task
                        .delegated_runs
                        .iter()
                        .any(|run| run.chat_id == parent.chat_id && run.run_id == parent.run_id))
                && !task
                    .delegated_runs
                    .iter()
                    .any(|run| run.run_id == child.run_id)
            {
                task.delegated_runs
                    .push(tickloop_harness_protocol::TaskDelegate {
                        chat_id: child.chat_id,
                        loop_id: child.loop_id,
                        run_id: child.run_id,
                    });
                changed = true;
            }
        }
        if changed {
            board.revision += 1;
            self.persist_task_boards(root).await?;
            self.publish_task_board(&mut chats, root_id, loop_id);
        }
        Ok(())
    }

    pub(super) async fn shared_task_tool(
        &self,
        receipt: &Receipt,
        operation: &OperationRecord,
        cancel: &CancellationToken,
    ) -> Result<ToolResult, String> {
        let name = operation.name.as_str();
        let ids = self.chat_ids(receipt.chat_id).await;
        let resolved_input = resolve_task_references(&ids, &operation.input)?;
        let input = &resolved_input;
        let snapshot = self
            .snapshot(receipt.chat_id)
            .await
            .map_err(|error| error.to_string())?;
        let ancestor_revisions =
            delegation::ancestor_revisions(&*self.inner.chats.lock().await, receipt.chat_id)?;
        let report_review = if name == "TaskUpdate"
            && input["action"] == "complete"
            && snapshot.subagent_reports.contains_key(&receipt.run_id)
        {
            Some(
                self.review_operation(receipt, operation, None, cancel.clone(), false)
                    .await
                    .map_err(|error| error.to_string())?
                    .details,
            )
        } else {
            None
        };
        let registrations = self
            .family_registrations(receipt.chat_id)
            .await
            .map_err(|error| error.to_string())?;
        self.refresh_task_tools_setting()
            .await
            .map_err(|error| error.to_string())?;
        if matches!(name, "TaskCreate" | "TaskUpdate") {
            // A member's call must survive before its effect is committed to the root's board.
            self.make_durable(&[receipt.chat_id]).await?;
        }
        let actor = TaskOwner {
            chat_id: receipt.chat_id,
            name: registrations
                .iter()
                .find(|entry| entry.child_chat_id == receipt.chat_id)
                .map_or("main", |entry| entry.handle.as_str())
                .into(),
        };
        let mut chats = self.inner.chats.lock().await;
        let (root_id, loop_id) = scope(&chats, receipt)?;
        let source = &chats[&receipt.chat_id];
        let ids = source.ids;
        if report_review.is_some()
            && (source.snapshot.policy.revision != snapshot.policy.revision
                || delegation::ancestor_revisions(&chats, receipt.chat_id)? != ancestor_revisions)
        {
            return Err(
                "permissions changed during task report review; retry at the current revision"
                    .into(),
            );
        }

        if cancel.is_cancelled()
            || source.family_stopping
            || source.active_run != Some(receipt.run_id)
        {
            return Err("task operation's Run is no longer active".into());
        }
        let access = tool_access(&chats, receipt);
        if access == TaskToolAccess::Disabled {
            return Err("Task tools are disabled; enable Task tools in Settings".into());
        }
        if access == TaskToolAccess::Finishing {
            let finishing_update = name == "TaskUpdate"
                && matches!(input["action"].as_str(), Some("complete" | "release"));
            if !matches!(name, "TaskGet" | "TaskList") && !finishing_update {
                return Err("Task tools are disabled; only completing or releasing your existing claim is available".into());
            }
            if finishing_update {
                let owned = chats[&root_id]
                    .snapshot
                    .task_boards
                    .get(&loop_id)
                    .is_some_and(|board| {
                        board.tasks.iter().any(|task| {
                            task.id.uuid().to_string()
                                == input["task_id"].as_str().unwrap_or_default()
                                && task.status == Status::InProgress
                                && task
                                    .owner
                                    .as_ref()
                                    .is_some_and(|owner| owner.chat_id == receipt.chat_id)
                                && task
                                    .claim
                                    .as_ref()
                                    .is_some_and(|claim| claim.run_id == receipt.run_id)
                        })
                    });
                if !owned {
                    return Err("only your existing claimed task can be finished while Task tools are disabled".into());
                }
            }
        }
        let claim = TaskClaim {
            run_id: receipt.run_id,
            generation: source.family_generation,
        };
        if chats[&root_id].task_checkpoint_pending {
            self.persist_task_boards(chats.get_mut(&root_id).unwrap())
                .await?;
            if chats[&root_id].snapshot.task_boards.contains_key(&loop_id) {
                self.publish_task_board(&mut chats, root_id, loop_id);
            }
        }
        let board = chats[&root_id]
            .snapshot
            .task_boards
            .get(&loop_id)
            .cloned()
            .unwrap_or_else(|| {
                let mut board = TaskBoard::new(root_id, loop_id);
                board.is_team = chats.values().any(|chat| {
                    chat.snapshot.delegation.as_ref().is_some_and(|parent| {
                        parent.root_chat_id() == root_id && parent.root_loop_id() == loop_id
                    })
                });
                board
            });
        if name == "TaskList" || name == "TaskGet" {
            let value = if name == "TaskList" {
                let pending_assignments = board.mutations.iter().filter(|(_, mutation)| mutation.notification.as_ref().is_some_and(|notification| !notification.delivered)).map(|(id, mutation)| json!({"mutation_id":id,"actor":mutation.actor,"task_id":mutation.task_id,"retry_input":mutation.input.strip_prefix("TaskUpdate:").and_then(|value|serde_json::from_str::<Value>(value).ok())})).collect::<Vec<_>>();
                let mut board = board.clone();
                board.mutations.clear();
                json!({"board":board,"pending_assignments":pending_assignments})
            } else {
                let id: Uuid = serde_json::from_value(input["task_id"].clone())
                    .map_err(|error| error.to_string())?;
                let task = board
                    .tasks
                    .iter()
                    .find(|task| task.id == id)
                    .ok_or_else(|| {
                        tickloop_ids::not_found::<SharedTaskKind>(
                            operation.input["task_id"].as_str().unwrap_or_default(),
                        )
                    })?;
                json!({"task":task,"blocked_by":board.blocked_by(task)})
            };
            return Ok(ToolResult {
                content: model_json(&ids, value).to_string(),
                is_error: false,
                output_ref: None,
            });
        }
        let mutation_id: Uuid = serde_json::from_value(input["mutation_id"].clone())
            .map_err(|error| error.to_string())?;
        let encoded = format!("{name}:{}", operation.input);
        if let Some(previous) = board.mutations.get(&mutation_id) {
            if previous.actor != receipt.chat_id || previous.input != encoded {
                return Err("mutation_id was already used for a different operation".into());
            }
            if previous.task_cleared {
                return Ok(ToolResult {
                    content: json!({"status":"already_applied","task":null,"task_cleared":true})
                        .to_string(),
                    is_error: false,
                    output_ref: None,
                });
            }
            let task = board
                .tasks
                .iter()
                .find(|task| task.id == previous.task_id)
                .cloned();
            drop(chats);
            let delivery = self
                .deliver_task_assignment(receipt, root_id, loop_id, mutation_id, cancel)
                .await;
            return Ok(ToolResult {
                content: model_json(
                    &ids,
                    json!({"status":"already_applied","task":task,"notification":delivery}),
                )
                .to_string(),
                is_error: false,
                output_ref: None,
            });
        }
        if board.mutations.len() >= MAX_TASK_MUTATIONS {
            return Err("task mutation limit reached for this Loop".into());
        }
        let mut candidate = board.clone();
        let mut notify = None;
        let task_id = match name {
            "TaskCreate" => {
                let create: CreateInput =
                    serde_json::from_value(input.clone()).map_err(|error| error.to_string())?;
                debug_assert_eq!(create.mutation_id, mutation_id);
                if candidate.tasks.len() >= MAX_TASKS {
                    return Err(format!("task board is limited to {MAX_TASKS} tasks"));
                }
                candidate
                    .task_number_offset
                    .checked_add(candidate.tasks.len() as u64)
                    .and_then(|number| number.checked_add(1))
                    .ok_or("task numbering exhausted")?;
                text_valid(&create.title)?;
                text_valid(&create.description)?;
                let id = tickloop_ids::generate();
                validate_dependencies(&candidate, id.into(), &create.dependencies)?;
                candidate.tasks.push(SharedTask {
                    id: id.into(),
                    title: create.title,
                    description: create.description,
                    dependencies: create.dependencies,
                    status: Status::Pending,
                    owner: None,
                    claim: None,
                    delegated_runs: Vec::new(),
                    revision: 1,
                    result: None,
                    report: None,
                });
                candidate.revision += 1;
                id
            }
            "TaskUpdate" => {
                let update: UpdateInput =
                    serde_json::from_value(input.clone()).map_err(|error| error.to_string())?;
                debug_assert_eq!(update.mutation_id, mutation_id);
                let assignment = if let UpdateAction::Assign { owner } = &update.action {
                    let target = if owner == "main" {
                        TaskOwner {
                            chat_id: root_id,
                            name: "main".into(),
                        }
                    } else {
                        let entry = registrations
                            .iter()
                            .find(|entry| {
                                entry.handle == *owner
                                    || ids
                                        .resolve(&ModelRef::<ChatKind>::from(owner.as_str()))
                                        .is_ok_and(|id| id == entry.child_chat_id)
                            })
                            .ok_or("unknown family member")?;
                        TaskOwner {
                            chat_id: entry.child_chat_id,
                            name: entry.handle.clone(),
                        }
                    };
                    let target_chat = chats
                        .get(&target.chat_id)
                        .ok_or("assignment target is unavailable")?;
                    let target_scope = target_chat
                        .snapshot
                        .delegation
                        .as_ref()
                        .map_or((target.chat_id, receipt.loop_id), |delegation| {
                            (delegation.root_chat_id(), delegation.root_loop_id())
                        });
                    if target_scope != (root_id, loop_id) || target_chat.family_stopping {
                        return Err(
                            "assignment target belongs to another root Loop or is stopping".into(),
                        );
                    }
                    if target.chat_id != receipt.chat_id {
                        notify = Some(target.clone());
                    }
                    Some(target)
                } else {
                    None
                };
                if matches!(
                    update.action,
                    UpdateAction::Complete { .. } | UpdateAction::Release { .. }
                ) {
                    let task = candidate
                        .tasks
                        .iter()
                        .find(|task| task.id == update.task_id)
                        .ok_or("unknown task")?;
                    ensure_execution_settled(&chats, task, receipt, operation.id)?;
                }
                // A conflict names the task as it is now, written as the model reads ids.
                let id = apply_update(&mut candidate, update, &actor, claim, assignment).map_err(
                    |error| match serde_json::from_str::<Value>(&error) {
                        Ok(conflict) => model_json(&ids, conflict).to_string(),
                        Err(_) => error,
                    },
                )?;
                if input["action"] == "complete" {
                    if let Some(review) = &report_review {
                        let task = candidate
                            .tasks
                            .iter_mut()
                            .find(|task| task.id == id)
                            .unwrap();
                        task.result = Some(handback::reviewed_report(
                            task.result.as_deref().unwrap_or_default(),
                            review,
                        ));
                    }
                    candidate
                        .tasks
                        .iter_mut()
                        .find(|task| task.id == id)
                        .unwrap()
                        .report = Some(TaskReport {
                        chat_id: receipt.chat_id,
                        run_id: receipt.run_id,
                        call_id: operation.call_id.clone(),
                        review: report_review,
                    });
                }
                id
            }
            _ => return Err("unknown shared task tool".into()),
        };
        let task = candidate
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .unwrap()
            .clone();
        let notification = notify.map(|target| TaskAssignment { target: target.chat_id, message: format!("Shared task assignment: {}. Claim at its current revision before starting. Stay within your existing authorized scope. Task: {}", task.title, model_json(&ids, json!(task))), delivered: false });
        candidate.mutations.insert(
            mutation_id,
            TaskMutationReceipt {
                actor: receipt.chat_id,
                input: encoded,
                task_id: task_id.into(),
                task_cleared: false,
                notification,
            },
        );
        let root = chats.get_mut(&root_id).unwrap();
        root.snapshot.task_boards.insert(loop_id, candidate);
        self.persist_task_boards(root).await?;
        self.publish_task_board(&mut chats, root_id, loop_id);
        drop(chats);
        let delivery = self
            .deliver_task_assignment(receipt, root_id, loop_id, mutation_id, cancel)
            .await;
        Ok(ToolResult {
            content: model_json(
                &ids,
                json!({"status":"applied","task":task,"notification":delivery}),
            )
            .to_string(),
            is_error: false,
            output_ref: None,
        })
    }

    async fn deliver_task_assignment(
        &self,
        receipt: &Receipt,
        root_id: ChatId,
        loop_id: LoopId,
        mutation_id: Uuid,
        cancel: &CancellationToken,
    ) -> String {
        if let Err(error) = self.refresh_task_tools_setting().await {
            return error.to_string();
        }
        let (notification, assistant_id, registered, ids) = {
            let chats = self.inner.chats.lock().await;
            if !chats[&root_id].snapshot.task_tools_enabled {
                return "Task tools are disabled; assignment notification retained without delivery".into();
            }
            let Some(notification) = chats[&root_id].snapshot.task_boards[&loop_id].mutations
                [&mutation_id]
                .notification
                .clone()
            else {
                return "not_needed".into();
            };
            if notification.delivered {
                return "delivered".into();
            }
            let board = &chats[&root_id].snapshot.task_boards[&loop_id];
            let task_id = board.mutations[&mutation_id].task_id;
            if board
                .tasks
                .iter()
                .find(|task| task.id == task_id)
                .is_none_or(|task| {
                    task.owner
                        .as_ref()
                        .is_none_or(|owner| owner.chat_id != notification.target)
                        || matches!(task.status, Status::Cancelled | Status::Completed)
                })
            {
                return "assignment superseded; notification not sent".into();
            }

            let chat = &chats[&receipt.chat_id];
            let assistant_id = chat
                .snapshot
                .message_ids
                .iter()
                .rev()
                .find(|(index, _)| chat.snapshot.messages[**index].role == Role::Assistant)
                .map_or(Uuid::nil(), |(_, id)| *id);
            let registered = tool_execution::ToolRegistry::for_chat(
                chat,
                None,
                false,
                self.inner.host_options.prompt_policy,
                TaskToolAccess::Disabled,
                &self.inner.tool_policy,
                self.scheduled_task_terms(),
            )
            .resolve(tickloop_harness_protocol::SEND_AGENT_MESSAGE_TOOL_NAME);
            (notification, assistant_id, registered, chat.ids)
        };
        let invocation = ToolInvocation::new(
            format!("task-assignment-{mutation_id}"),
            tickloop_harness_protocol::SEND_AGENT_MESSAGE_TOOL_NAME.into(),
            json!({"to":ids.model_text(notification.target),"message":notification.message}),
            registered,
        );
        let result = match Box::pin(self.execute_registered_call(
            receipt,
            assistant_id,
            invocation,
            cancel.clone(),
        ))
        .await
        {
            Ok(outcome) if !outcome.result.is_error => outcome.result.content,
            Ok(outcome) => {
                if let Some(record) = outcome.denial {
                    self.record_final_denial(record).await;
                }
                return format!(
                    "assignment saved; notification pending: {}",
                    outcome.result.content
                );
            }
            Err(error) => {
                if let Some(record) = error.denial {
                    self.record_final_denial(*record).await;
                }
                return format!("assignment saved; notification pending: {}", error.message);
            }
        };
        let mut chats = self.inner.chats.lock().await;
        if let Some(target) = chats.get(&notification.target) {
            // Only acknowledge durable consumption. Queued inputs remain in the root outbox
            // until consumed, so a crash before delivery can be retried without rerunning work.
            let consumed = target
                .snapshot
                .agent_message_origins
                .values()
                .any(|source| source["message_id"] == mutation_id.to_string());
            if consumed {
                if let Err(error) = self
                    .checkpoint_transcript(&target.snapshot, &target.bookkeeping)
                    .await
                {
                    return format!("notification outcome unknown: {error}");
                }
                let root = chats.get_mut(&root_id).unwrap();
                let Some(notification) = root
                    .snapshot
                    .task_boards
                    .get_mut(&loop_id)
                    .and_then(|board| board.mutations.get_mut(&mutation_id))
                    .and_then(|mutation| mutation.notification.as_mut())
                else {
                    return result;
                };
                notification.delivered = true;
                if let Err(error) = self.persist_task_boards(root).await {
                    return error;
                }
            }
        }
        result
    }

    pub(super) async fn acknowledge_task_assignments(
        &self,
        receipt: &Receipt,
    ) -> Result<(), String> {
        let mut chats = self.inner.chats.lock().await;
        let (root_id, loop_id) = scope(&chats, receipt)?;
        let Some(board) = chats[&root_id].snapshot.task_boards.get(&loop_id) else {
            return Ok(());
        };
        let target = &chats[&receipt.chat_id];
        let consumed = board
            .mutations
            .iter()
            .filter(|(id, mutation)| {
                mutation.notification.as_ref().is_some_and(|notification| {
                    !notification.delivered && notification.target == receipt.chat_id
                }) && target
                    .snapshot
                    .agent_message_origins
                    .values()
                    .any(|source| source["message_id"] == id.to_string())
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        if consumed.is_empty() {
            return Ok(());
        }
        self.checkpoint_transcript(&target.snapshot, &target.bookkeeping)
            .await?;
        let root = chats.get_mut(&root_id).unwrap();
        let board = root.snapshot.task_boards.get_mut(&loop_id).unwrap();
        for id in consumed {
            board
                .mutations
                .get_mut(&id)
                .unwrap()
                .notification
                .as_mut()
                .unwrap()
                .delivered = true;
        }
        self.persist_task_boards(root).await
    }

    pub(super) async fn settle_task_claims(&self, receipt: &Receipt) {
        let mut chats = self.inner.chats.lock().await;
        let Ok((root_id, loop_id)) = scope(&chats, receipt) else {
            return;
        };
        let root = chats.get_mut(&root_id).unwrap();
        let Some(board) = root.snapshot.task_boards.get_mut(&loop_id) else {
            return;
        };
        let mut changed = false;
        for task in &mut board.tasks {
            if task.status == Status::InProgress
                && task
                    .owner
                    .as_ref()
                    .is_some_and(|owner| owner.chat_id == receipt.chat_id)
                && task
                    .claim
                    .as_ref()
                    .is_some_and(|claim| claim.run_id == receipt.run_id)
            {
                task.status = Status::NeedsAttention;
                task.result = Some("The claiming Run ended without completing or releasing the task. Reconcile its effects before explicit release.".into());
                task.revision += 1;
                board.revision += 1;
                changed = true;
            }
        }
        if changed {
            let _ = self.persist_task_boards(root).await;
            self.publish_task_board(&mut chats, root_id, loop_id);
        }
    }

    /// Reconnect claims only after every member's continuation or wait has been registered,
    /// before any Run starts. A claim whose execution did not come back needs reconciliation.
    pub(crate) async fn restore_task_claims(&self, members: &[ChatId]) {
        let mut chats = self.inner.chats.lock().await;
        let live = members
            .iter()
            .filter_map(|id| chats.get(id))
            .flat_map(|chat| {
                chat.ready_runs
                    .iter()
                    .chain(chat.parked.keys())
                    .copied()
                    .chain(chat.active_run)
                    .chain(
                        chat.snapshot
                            .pending_inputs
                            .iter()
                            .filter(|pending| pending.task_continuation)
                            .map(|pending| pending.receipt.run_id),
                    )
                    .map(|run| ((chat.snapshot.id, run), chat.family_generation))
            })
            .collect::<HashMap<_, _>>();
        for id in members {
            let Some(root) = chats.get_mut(id) else {
                continue;
            };
            let mut changed = false;
            for board in root.snapshot.task_boards.values_mut() {
                for task in &mut board.tasks {
                    if task.status != Status::InProgress {
                        continue;
                    }
                    let generation = task
                        .owner
                        .as_ref()
                        .zip(task.claim.as_ref())
                        .and_then(|(owner, claim)| live.get(&(owner.chat_id, claim.run_id)));
                    if let Some(generation) = generation {
                        let claim = task.claim.as_mut().unwrap();
                        changed |= claim.generation != *generation;
                        claim.generation = *generation;
                    } else {
                        task.status = Status::NeedsAttention;
                        task.result = Some("Worker restored; reconcile the prior execution before explicit release.".into());
                        task.revision += 1;
                        board.revision += 1;
                        changed = true;
                    }
                }
            }
            if changed {
                let _ = self.persist_task_boards(root).await;
            }
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod activation_tests;

#[cfg(test)]
mod cleanup_tests;

use super::*;
use tickloop_code_access::AgentCommand;
use tickloop_harness::protocol::{GoalAction, GoalSnapshot, GoalStatus};

const MAX_OBJECTIVE_CHARACTERS: usize = 4000;
const NO_GOAL: &str = "No Goal is set; use /goal CONDITION.";
const GOAL_ACHIEVED: &str = "Goal already achieved. Set a new one with /goal CONDITION.";
const BUDGET_EXHAUSTED: &str =
    "Goal hit its token budget. Raise the budget from /goal before resuming.";
const INVALID_BUDGET: &str =
    "Enter a positive token budget such as 50K or 1.5M, or none for no budget.";
const BUDGET_LABEL: &str = "Token budget (e.g. 50K, 1.5M, or none)";
const TOKENS_PER_K: u64 = 1_000;
const TOKENS_PER_M: u64 = 1_000_000;

#[derive(Clone)]
pub(super) enum GoalMenuAction {
    Command(GoalAction),
    Edit,
    SaveEdit,
    Budget,
    SaveBudget,
    Continue { resume_goal: bool },
    KeepPaused,
    KeepCurrent,
}

enum GoalCommand {
    Show,
    Edit,
    Apply(GoalAction),
}

fn parse(line: &str) -> Result<GoalCommand, String> {
    let argument = line
        .strip_prefix("/goal")
        .ok_or("Use /goal CONDITION")?
        .trim();
    Ok(match argument {
        "" => GoalCommand::Show,
        "edit" => GoalCommand::Edit,
        "pause" => GoalCommand::Apply(GoalAction::Pause),
        "resume" => GoalCommand::Apply(GoalAction::Resume),
        "clear" => GoalCommand::Apply(GoalAction::Clear),
        objective => {
            validate_objective(objective)?;
            GoalCommand::Apply(GoalAction::Set {
                objective: objective.into(),
                token_budget: None,
                replace: false,
            })
        }
    })
}

fn validate_objective(objective: &str) -> Result<(), String> {
    if objective.trim().is_empty() {
        return Err("Enter a Goal condition.".into());
    }
    if objective.chars().count() > MAX_OBJECTIVE_CHARACTERS {
        return Err("Goal conditions are limited to 4000 characters; put longer content in a file or Artifact and reference it.".into());
    }
    Ok(())
}

fn budget(value: &str) -> Result<Option<u64>, String> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    let (number, multiplier) = match value.as_bytes().last() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], TOKENS_PER_K),
        Some(b'm' | b'M') => (&value[..value.len() - 1], TOKENS_PER_M),
        _ => (value, TOKENS_PER_K),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || number.ends_with('.')
    {
        return Err(INVALID_BUDGET.into());
    }
    let fraction = fraction.trim_end_matches('0');
    let denominator = 10_u64
        .checked_pow(u32::try_from(fraction.len()).unwrap_or(u32::MAX))
        .filter(|denominator| *denominator <= multiplier)
        .ok_or(INVALID_BUDGET)?;
    let whole = whole
        .parse::<u64>()
        .ok()
        .and_then(|whole| whole.checked_mul(multiplier))
        .ok_or(INVALID_BUDGET)?;
    let fractional = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>().map_err(|_| INVALID_BUDGET)? * (multiplier / denominator)
    };
    whole
        .checked_add(fractional)
        .filter(|value| *value > 0)
        .map(Some)
        .ok_or_else(|| INVALID_BUDGET.into())
}

fn budget_text(value: u64) -> String {
    let (divisor, unit) = if value >= TOKENS_PER_M {
        (TOKENS_PER_M, "M")
    } else {
        (TOKENS_PER_K, "K")
    };
    let whole = value / divisor;
    let remainder = value % divisor;
    if remainder == 0 {
        return format!("{whole}{unit}");
    }
    let digits = divisor.ilog10() as usize;
    let fraction = format!("{remainder:0digits$}");
    format!("{whole}.{}{unit}", fraction.trim_end_matches('0'))
}

fn action_refusal(goal: Option<&GoalSnapshot>, action: &GoalAction) -> Option<&'static str> {
    if let GoalAction::Set { replace, .. } = action {
        return goal
            .filter(|goal| !*replace && !goal.status.is_finished())
            .map(|_| "An unfinished Goal is already set. Open /goal to edit or clear it.");
    }
    let Some(goal) = goal else {
        return Some(NO_GOAL);
    };
    if matches!(action, GoalAction::Clear) {
        return None;
    }
    if goal.status == GoalStatus::Complete {
        return Some(GOAL_ACHIEVED);
    }
    match action {
        GoalAction::Resume if goal.status == GoalStatus::Active => Some("Goal is already active."),
        GoalAction::Resume
            if goal
                .token_budget
                .is_some_and(|budget| goal.tokens_used >= budget) =>
        {
            Some(BUDGET_EXHAUSTED)
        }
        GoalAction::Pause => match goal.status {
            GoalStatus::Active => None,
            GoalStatus::Paused { .. } => Some("Goal is already paused."),
            GoalStatus::Blocked { .. } => {
                Some("Goal is stalled. Resume it from /goal before pausing.")
            }
            GoalStatus::BudgetLimited => Some(BUDGET_EXHAUSTED),
            GoalStatus::Complete => unreachable!(),
        },
        GoalAction::SetBudget {
            token_budget: Some(0),
        } => Some(INVALID_BUDGET),
        _ => None,
    }
}

async fn apply(agent: &Agent, chat: Uuid, action: GoalAction) -> Result<(), String> {
    let status = agent
        .status(chat)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(reason) = action_refusal(status.goal.as_ref(), &action) {
        return Err(reason.into());
    }
    let execution = agent
        .reference(chat)
        .await
        .map_err(|error| error.to_string())?;
    if let Err(error) = agent
        .command(
            AgentCommand::ManageGoal {
                execution,
                action: action.clone(),
            },
            None,
        )
        .await
    {
        if error.code == tickloop_code_access::AccessErrorCode::Invalid {
            let latest = agent.status(chat).await.ok();
            return Err(latest
                .as_ref()
                .and_then(|status| action_refusal(status.goal.as_ref(), &action))
                .unwrap_or("Goal could not be changed. Open /goal to check its current state.")
                .into());
        }
        return Err(error.to_string());
    }
    Ok(())
}

impl Menus {
    pub(in crate::terminal) async fn goal_command(
        &mut self,
        agent: &Agent,
        console: &Console,
        chat: Uuid,
        line: &str,
    ) -> Result<(), String> {
        let command = parse(line)?;
        let snapshot = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        if snapshot.delegation.is_some() {
            return Err("Only the root Agent owns a Goal.".into());
        }
        match command {
            GoalCommand::Show => self.goal_summary(console, chat, snapshot.goal.as_ref()),
            GoalCommand::Edit => {
                self.goal_edit(console, chat, snapshot.goal.as_ref().ok_or(NO_GOAL)?)
            }
            GoalCommand::Apply(GoalAction::Set {
                objective,
                token_budget,
                ..
            }) if snapshot
                .goal
                .as_ref()
                .is_some_and(|goal| !goal.status.is_finished()) =>
            {
                self.show(
                    console,
                    Menu {
                        id: tickloop_ids::generate(),
                        title: "Replace unfinished Goal?".into(),
                        context: vec![
                            safe_text(&format!(
                                "Current: {}",
                                snapshot.goal.as_ref().unwrap().objective
                            )),
                            safe_text(&format!("New: {objective}")),
                        ],
                        markdown: None,
                        options: vec!["Replace Goal".into(), "Keep current Goal".into()],
                        selected: 0,
                        confirmation: true,
                    },
                    Selection::Goal {
                        chat,
                        actions: vec![
                            GoalMenuAction::Command(GoalAction::Set {
                                objective,
                                token_budget,
                                replace: true,
                            }),
                            GoalMenuAction::KeepCurrent,
                        ],
                    },
                )
            }
            GoalCommand::Apply(action) => apply(agent, chat, action).await,
        }
    }

    fn goal_summary(
        &mut self,
        console: &Console,
        chat: Uuid,
        goal: Option<&GoalSnapshot>,
    ) -> Result<(), String> {
        let Some(goal) = goal else {
            console.set_status(NO_GOAL)?;
            return console.write("No Goal is set; use /goal CONDITION.\n");
        };
        let context = super::super::goal_view::GoalView::from_snapshot(goal)
            .detail_lines(100, 0, usize::MAX)
            .iter()
            .skip(1)
            .map(|line| safe_text(&line.text()))
            .collect();
        if goal.status.is_finished() {
            return self.show(
                console,
                Menu {
                    id: tickloop_ids::generate(),
                    title: "Goal".into(),
                    context,
                    markdown: None,
                    options: vec!["Clear".into()],
                    selected: 0,
                    confirmation: false,
                },
                Selection::Goal {
                    chat,
                    actions: vec![GoalMenuAction::Command(GoalAction::Clear)],
                },
            );
        }
        let (label, action) = match goal.status {
            GoalStatus::Active => ("Pause", GoalAction::Pause),
            GoalStatus::Paused { .. }
            | GoalStatus::Blocked { .. }
            | GoalStatus::BudgetLimited
            | GoalStatus::Complete => ("Resume", GoalAction::Resume),
        };
        let (options, actions) = if goal.status != GoalStatus::Active
            && goal
                .token_budget
                .is_some_and(|budget| goal.tokens_used >= budget)
        {
            (
                vec![
                    "Adjust token budget".into(),
                    "Edit · ctrl+g external editor".into(),
                    "Clear".into(),
                ],
                vec![
                    GoalMenuAction::Budget,
                    GoalMenuAction::Edit,
                    GoalMenuAction::Command(GoalAction::Clear),
                ],
            )
        } else {
            (
                vec![
                    "Edit · ctrl+g external editor".into(),
                    label.into(),
                    "Clear".into(),
                    "Adjust token budget".into(),
                ],
                vec![
                    GoalMenuAction::Edit,
                    GoalMenuAction::Command(action),
                    GoalMenuAction::Command(GoalAction::Clear),
                    GoalMenuAction::Budget,
                ],
            )
        };
        self.show(
            console,
            Menu {
                id: tickloop_ids::generate(),
                title: "Goal".into(),
                context,
                markdown: None,
                options,
                selected: 0,
                confirmation: false,
            },
            Selection::Goal { chat, actions },
        )
    }

    fn goal_edit(
        &mut self,
        console: &Console,
        chat: Uuid,
        goal: &GoalSnapshot,
    ) -> Result<(), String> {
        if goal.status == GoalStatus::Complete {
            return Err(GOAL_ACHIEVED.into());
        }
        self.goal_input(
            console,
            chat,
            "Edit Goal",
            "Condition",
            &goal.objective,
            GoalMenuAction::SaveEdit,
        )
    }

    fn goal_input(
        &mut self,
        console: &Console,
        chat: Uuid,
        title: &str,
        label: &'static str,
        value: &str,
        action: GoalMenuAction,
    ) -> Result<(), String> {
        let menu = Menu {
            id: tickloop_ids::generate(),
            title: title.into(),
            context: vec!["ctrl+g external editor · enter save · esc cancel".into()],
            markdown: None,
            options: vec!["Save".into()],
            selected: 0,
            confirmation: false,
        };
        let selection = Selection::Goal {
            chat,
            actions: vec![action],
        };
        if !console.is_interactive() {
            return self.show(console, menu, selection);
        }
        if self.prepare_dialog(console, None)? == RequestPresentation::Deferred {
            return Ok(());
        }
        let id = menu.id;
        console.show_input_menu(menu, label, value)?;
        self.active = Some((id, selection));
        Ok(())
    }

    pub(in crate::terminal) fn reoffer_continue(&mut self, chat: Uuid) {
        self.continue_offered.remove(&chat);
    }

    pub(in crate::terminal) fn offer_continue(
        &mut self,
        console: &Console,
        status: &ChatStatus,
    ) -> Result<(), String> {
        self.sync_dialog_chat(console)?;
        if !console.is_interactive()
            || self.id().is_some()
            || self.continue_offered.contains(&status.id.uuid())
        {
            return Ok(());
        }
        let receiving_paused = !status.chat_loop.receiving.is_receiving();
        let resume_goal = status.goal.as_ref().is_some_and(|goal| {
            matches!(
                goal.status,
                GoalStatus::Paused { .. } | GoalStatus::Blocked { .. }
            )
        });
        if !receiving_paused && !resume_goal {
            self.continue_offered.insert(status.id.into());
            return Ok(());
        }
        let mut context = vec![if receiving_paused {
            "Trigger receiving is paused.".into()
        } else {
            "Trigger receiving is active.".into()
        }];
        if let Some(goal) = status.goal.as_ref().filter(|_| resume_goal) {
            context.push(safe_text(&format!("Goal: {}", goal.objective)));
        }
        self.continue_offered.insert(status.id.into());
        self.show(
            console,
            Menu {
                id: tickloop_ids::generate(),
                title: "Continue Chat?".into(),
                context,
                markdown: None,
                options: vec![
                    match (receiving_paused, resume_goal) {
                        (true, true) => "Continue receiving and resume Goal",
                        (true, false) => "Continue receiving",
                        (false, true) => "Resume Goal",
                        (false, false) => unreachable!(),
                    }
                    .into(),
                    "Keep paused".into(),
                ],
                selected: 0,
                confirmation: false,
            },
            Selection::Goal {
                chat: status.id.into(),
                actions: vec![
                    GoalMenuAction::Continue { resume_goal },
                    GoalMenuAction::KeepPaused,
                ],
            },
        )
    }

    pub(super) async fn choose_goal(
        &mut self,
        agent: &Agent,
        console: &Console,
        chat: Uuid,
        actions: &[GoalMenuAction],
        index: Option<usize>,
        comment: Option<String>,
    ) -> Result<MenuOutcome, String> {
        let Some(action) = index.and_then(|index| actions.get(index)) else {
            return Ok(MenuOutcome::default());
        };
        match action {
            GoalMenuAction::Command(action) => apply(agent, chat, action.clone()).await?,
            GoalMenuAction::Edit | GoalMenuAction::Budget => {
                let snapshot = agent.snapshot(chat).await.map_err(|error| error.to_string())?;
                let goal = snapshot.goal.as_ref().ok_or(NO_GOAL)?;
                if goal.status == GoalStatus::Complete { return Err(GOAL_ACHIEVED.into()); }
                if matches!(action, GoalMenuAction::Edit) { self.goal_edit(console, chat, goal)?; }
                else { self.goal_input(console, chat, "Goal token budget", BUDGET_LABEL, &goal.token_budget.map_or_else(|| "none".into(), budget_text), GoalMenuAction::SaveBudget)?; }
            }
            GoalMenuAction::SaveEdit => {
                let objective = comment.ok_or("Enter a Goal condition.")?;
                validate_objective(&objective)?;
                apply(agent, chat, GoalAction::Edit { objective }).await?;
            }
            GoalMenuAction::SaveBudget => apply(agent, chat, GoalAction::SetBudget { token_budget: budget(comment.as_deref().unwrap_or_default())? }).await?,
            GoalMenuAction::Continue { resume_goal } => {
                let status = agent.status(chat).await.map_err(|error| error.to_string())?;
                if !status.chat_loop.receiving.is_receiving() {
                    let execution = agent.reference(chat).await.map_err(|error| error.to_string())?;
                    agent.command(AgentCommand::ContinueLoop { execution, origin: agent.origin() }, None).await.map_err(|error| error.to_string())?;
                }
                if *resume_goal { apply(agent, chat, GoalAction::Resume).await?; }
            }
            GoalMenuAction::KeepCurrent => return Ok(MenuOutcome::default()),
            GoalMenuAction::KeepPaused => return Ok(MenuOutcome { status: Some("Kept unchanged · /goal resume resumes the Goal; send a message to resume receiving.".into()), ..Default::default() }),
        }
        Ok(MenuOutcome::default())
    }
}

#[cfg(test)]
mod tests;

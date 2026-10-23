use super::*;
use tickloop_agent_protocol::{AgentCommand, GoalAction, GoalSnapshot, GoalStatus};

const MAX_OBJECTIVE_CHARACTERS: usize = 4000;

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
    match value.trim() {
        "none" => Ok(None),
        value => value
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .map(Some)
            .ok_or_else(|| "Enter a positive token count, or none to remove the budget.".into()),
    }
}

async fn apply(agent: &Agent, chat: Uuid, action: GoalAction) -> Result<(), String> {
    let execution = agent
        .reference(chat)
        .await
        .map_err(|error| error.to_string())?;
    agent
        .command(AgentCommand::ManageGoal { execution, action }, None)
        .await
        .map_err(|error| error.to_string())?;
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
            GoalCommand::Edit => self.goal_edit(
                console,
                chat,
                snapshot
                    .goal
                    .as_ref()
                    .ok_or("No Goal is set; use /goal CONDITION.")?,
            ),
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
                        id: Uuid::new_v4(),
                        title: "Replace unfinished Goal?".into(),
                        context: vec![
                            format!("Current: {}", snapshot.goal.as_ref().unwrap().objective),
                            format!("New: {objective}"),
                        ],
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
            console.set_status("No Goal is set; use /goal CONDITION.")?;
            return console.write("No Goal is set; use /goal CONDITION.\n");
        };
        let context = super::super::goal_view::GoalView::from_snapshot(goal)
            .detail_lines(100, 0, usize::MAX)
            .iter()
            .skip(1)
            .map(|line| line.text())
            .collect();
        if goal.status.is_finished() {
            return self.show(
                console,
                Menu {
                    id: Uuid::new_v4(),
                    title: "Goal".into(),
                    context,
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
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Goal".into(),
                context,
                options: vec![
                    "Edit · Ctrl+G external editor".into(),
                    label.into(),
                    "Clear".into(),
                    "Adjust token budget".into(),
                ],
                selected: 0,
                confirmation: false,
            },
            Selection::Goal {
                chat,
                actions: vec![
                    GoalMenuAction::Edit,
                    GoalMenuAction::Command(action),
                    GoalMenuAction::Command(GoalAction::Clear),
                    GoalMenuAction::Budget,
                ],
            },
        )
    }

    fn goal_edit(
        &mut self,
        console: &Console,
        chat: Uuid,
        goal: &GoalSnapshot,
    ) -> Result<(), String> {
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
            id: Uuid::new_v4(),
            title: title.into(),
            context: vec!["Ctrl+G external editor · Enter save · Esc cancel".into()],
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
            || self.continue_offered.contains(&status.id)
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
            self.continue_offered.insert(status.id);
            return Ok(());
        }
        let mut context = vec![if receiving_paused {
            "Trigger receiving is paused.".into()
        } else {
            "Trigger receiving is active.".into()
        }];
        if let Some(goal) = status.goal.as_ref().filter(|_| resume_goal) {
            context.push(format!("Goal: {}", goal.objective));
        }
        self.continue_offered.insert(status.id);
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Continue Chat?".into(),
                context,
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
                confirmation: true,
            },
            Selection::Goal {
                chat: status.id,
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
                let goal = snapshot.goal.as_ref().ok_or("Goal was cleared.")?;
                if matches!(action, GoalMenuAction::Edit) { self.goal_edit(console, chat, goal)?; }
                else { self.goal_input(console, chat, "Goal token budget", "Tokens (or none)", &goal.token_budget.map_or_else(|| "none".into(), |value| value.to_string()), GoalMenuAction::SaveBudget)?; }
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

//! The root Agent's goal tools, adapted from Codex's `ext/goal/src/spec.rs`. Only the user
//! pauses, resumes, edits or clears a goal, so UpdateGoal reports just `complete` with evidence or
//! `blocked` with a reason.

use serde::Deserialize;
use serde_json::{Value, json};
use tickloop_agent_protocol::ToolDefinition;

pub(crate) const GET_GOAL_TOOL_NAME: &str = "GetGoal";
pub(crate) const CREATE_GOAL_TOOL_NAME: &str = "CreateGoal";
pub(crate) const UPDATE_GOAL_TOOL_NAME: &str = "UpdateGoal";
pub(crate) const MAX_OBJECTIVE_CHARACTERS: usize = 4000;

/// A goal CreateGoal asks the host to set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NewGoal {
    pub objective: String,
    /// `None` leaves the Settings default in place.
    pub token_budget: Option<u64>,
}

/// What UpdateGoal reports about the active goal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GoalReport {
    /// Awaits the host's completion check before the goal becomes complete.
    Complete {
        evidence: String,
    },
    Blocked {
        reason: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetGoalInput {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateGoalInput {
    objective: String,
    token_budget: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReportedStatus {
    Complete,
    Blocked,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateGoalInput {
    status: ReportedStatus,
    evidence: Option<String>,
    reason: Option<String>,
}

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: GET_GOAL_TOOL_NAME.into(),
            description: "Read this Chat's goal: its objective, status, token budget and usage, elapsed time, and the reason from the last completion check that did not accept a completion report. Reports when no goal is set.".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        },
        ToolDefinition {
            name: CREATE_GOAL_TOOL_NAME.into(),
            description: format!("Set a goal for this Chat only when the user explicitly asks you to set or pursue a goal; never infer a goal from an ordinary task. The host then keeps continuing toward the objective across turns until a completion check verifies it. Fails if the Chat already has an unfinished goal; the user replaces or clears it with /goal. Set token_budget only when the user asks for a token budget; otherwise the Settings default applies. Use {UPDATE_GOAL_TOOL_NAME} only to report completion or a blocker."),
            input_schema: json!({"type":"object","properties":{
                "objective":{"type":"string","minLength":1,"maxLength":MAX_OBJECTIVE_CHARACTERS,"description":format!("The concrete completion condition, at most {MAX_OBJECTIVE_CHARACTERS} characters. Put longer material in a file or Artifact and reference it.")},
                "token_budget":{"type":"integer","minimum":1,"description":"Token budget for the goal, counted across this Chat's whole Agent family. Omit unless the user asked for one."}
            },"required":["objective"],"additionalProperties":false}),
        },
        ToolDefinition {
            name: UPDATE_GOAL_TOOL_NAME.into(),
            description: "Report the outcome of this Chat's active goal.
Set status `complete` only when the objective has actually been achieved and no required work remains, with evidence that walks through the requirements one by one and points to results already shown in this conversation. The host then runs an independent completion check that sees only the objective, your report and this conversation: it runs no tools and does not assume results that are not shown. If the check does not accept the report, the goal stays active and the reason comes back with the next continuation. Do not report completion merely because the budget is nearly exhausted or because you are stopping work. When the goal has a token budget and the result reports it complete, tell the user the final token usage.
Set status `blocked`, with the blocking condition as reason, only when the same condition has persisted through at least three consecutive automatic goal continuations, not counting the user-triggered turn that set or resumed the goal, and you cannot make meaningful progress without user input or an external-state change. After the user resumes a blocked goal, start a fresh blocked audit. Once the threshold is met, report `blocked` instead of continuing to say you are blocked. Never use `blocked` merely because the work is hard, slow, uncertain, incomplete, or would benefit from clarification.
You cannot pause, resume, edit, clear, or budget-limit a goal; the user and the host control those.".into(),
            input_schema: json!({"type":"object","properties":{
                "status":{"type":"string","enum":["complete","blocked"]},
                "evidence":{"type":"string","minLength":1,"description":"Required with `complete`: requirement-by-requirement evidence from this conversation."},
                "reason":{"type":"string","minLength":1,"description":"Required with `blocked`: the blocking condition and what would unblock it."}
            },"required":["status"],"additionalProperties":false}),
        },
    ]
}

pub(crate) fn parse_get_goal(input: &Value) -> Result<(), String> {
    GetGoalInput::deserialize(input)
        .map(|_| ())
        .map_err(|error| invalid(GET_GOAL_TOOL_NAME, error))
}

pub(crate) fn parse_create_goal(input: &Value) -> Result<NewGoal, String> {
    let input = CreateGoalInput::deserialize(input)
        .map_err(|error| invalid(CREATE_GOAL_TOOL_NAME, error))?;
    let objective = input.objective.trim();
    if objective.is_empty() {
        return Err(invalid(CREATE_GOAL_TOOL_NAME, "objective is empty"));
    }
    if objective.chars().count() > MAX_OBJECTIVE_CHARACTERS {
        return Err(invalid(
            CREATE_GOAL_TOOL_NAME,
            format!(
                "objective exceeds {MAX_OBJECTIVE_CHARACTERS} characters; put longer material in a file or Artifact and reference it"
            ),
        ));
    }
    if input.token_budget == Some(0) {
        return Err(invalid(
            CREATE_GOAL_TOOL_NAME,
            "token_budget must be positive",
        ));
    }
    Ok(NewGoal {
        objective: objective.into(),
        token_budget: input.token_budget,
    })
}

pub(crate) fn parse_update_goal(input: &Value) -> Result<GoalReport, String> {
    let input = UpdateGoalInput::deserialize(input)
        .map_err(|error| invalid(UPDATE_GOAL_TOOL_NAME, error))?;
    let text = |field: Option<String>, name: &str, status: &str| match field {
        Some(text) if !text.trim().is_empty() => Ok(text.trim().to_owned()),
        _ => Err(invalid(
            UPDATE_GOAL_TOOL_NAME,
            format!("status `{status}` requires {name}"),
        )),
    };
    let unexpected = |name: &str, status: &str| {
        Err(invalid(
            UPDATE_GOAL_TOOL_NAME,
            format!("status `{status}` does not take {name}"),
        ))
    };
    match input.status {
        ReportedStatus::Complete if input.reason.is_some() => unexpected("reason", "complete"),
        ReportedStatus::Complete => Ok(GoalReport::Complete {
            evidence: text(input.evidence, "evidence", "complete")?,
        }),
        ReportedStatus::Blocked if input.evidence.is_some() => unexpected("evidence", "blocked"),
        ReportedStatus::Blocked => Ok(GoalReport::Blocked {
            reason: text(input.reason, "reason", "blocked")?,
        }),
    }
}

fn invalid(tool: &str, reason: impl std::fmt::Display) -> String {
    format!("invalid {tool} input: {reason}")
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;

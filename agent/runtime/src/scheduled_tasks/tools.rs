//! The root Agent's scheduled task tools, adapted from Claude Code's CronCreate, CronList,
//! CronDelete and ScheduleWakeup.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tickloop_agent_protocol::ToolDefinition;

use super::tasks::{MAX_WAKEUP_DELAY, MIN_WAKEUP_DELAY, TASK_LIFETIME};
use crate::host_tools::HostToolPermission;
use tickloop_agent_config::loop_skill::DEFAULT_INSTRUCTIONS_PROMPT as LOOP_INSTRUCTIONS_PLACEHOLDER;

pub(crate) const CRON_CREATE_TOOL_NAME: &str = "CronCreate";
pub(crate) const CRON_LIST_TOOL_NAME: &str = "CronList";
pub(crate) const CRON_DELETE_TOOL_NAME: &str = "CronDelete";
pub(crate) const SCHEDULE_WAKEUP_TOOL_NAME: &str = "ScheduleWakeup";

const LIFETIME_DAYS: u64 = TASK_LIFETIME.as_secs() / (24 * 60 * 60);

pub(crate) fn is_scheduled_task_tool(name: &str) -> bool {
    matches!(
        name,
        CRON_CREATE_TOOL_NAME
            | CRON_LIST_TOOL_NAME
            | CRON_DELETE_TOOL_NAME
            | SCHEDULE_WAKEUP_TOOL_NAME
    )
}

/// Listing is read-only; creating, deleting and rescheduling are Agent actions reviewed in Auto
/// and Plan like a Subagent, and need no confirmation of their own: a host without permission
/// requests has the user confirm the tasks the Agent creates instead.
pub(crate) fn permission(name: &str) -> HostToolPermission {
    match name {
        CRON_LIST_TOOL_NAME => HostToolPermission::ReadOnly,
        _ => HostToolPermission::Classified {
            allow_rule_bypasses_review: false,
        },
    }
}

pub(crate) fn is_concurrency_safe(name: &str) -> bool {
    name == CRON_LIST_TOOL_NAME
}

/// One scheduled task tool call, read from its input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScheduledTaskCall {
    Create {
        cron: String,
        prompt: String,
        recurring: bool,
    },
    List,
    Delete {
        id: String,
    },
    Wakeup {
        delay: Duration,
        reason: String,
        prompt: String,
    },
    StopWakeup,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateInput {
    cron: String,
    prompt: String,
    #[serde(default = "recurring_by_default")]
    recurring: bool,
}

fn recurring_by_default() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListInput {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteInput {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WakeupInput {
    delay_seconds: Option<u64>,
    reason: Option<String>,
    prompt: Option<String>,
    #[serde(default)]
    stop: bool,
}

pub(crate) fn parse(name: &str, input: &Value) -> Result<ScheduledTaskCall, String> {
    let invalid = |error: serde_json::Error| format!("invalid {name} input: {error}");
    match name {
        CRON_CREATE_TOOL_NAME => {
            let input: CreateInput = serde_json::from_value(input.clone()).map_err(invalid)?;
            if input.prompt.trim().is_empty() {
                return Err(format!("{name} needs a prompt to run"));
            }
            Ok(ScheduledTaskCall::Create {
                cron: input.cron,
                prompt: input.prompt,
                recurring: input.recurring,
            })
        }
        CRON_LIST_TOOL_NAME => {
            serde_json::from_value::<ListInput>(input.clone()).map_err(invalid)?;
            Ok(ScheduledTaskCall::List)
        }
        CRON_DELETE_TOOL_NAME => {
            let input: DeleteInput = serde_json::from_value(input.clone()).map_err(invalid)?;
            Ok(ScheduledTaskCall::Delete { id: input.id })
        }
        SCHEDULE_WAKEUP_TOOL_NAME => {
            let input: WakeupInput = serde_json::from_value(input.clone()).map_err(invalid)?;
            if input.stop {
                return Ok(ScheduledTaskCall::StopWakeup);
            }
            match (input.delay_seconds, input.reason, input.prompt) {
                (Some(delay_seconds), Some(reason), Some(prompt))
                    if !reason.trim().is_empty() && !prompt.trim().is_empty() =>
                {
                    Ok(ScheduledTaskCall::Wakeup {
                        delay: Duration::from_secs(delay_seconds),
                        reason,
                        prompt,
                    })
                }
                _ => Err(
                    "`delaySeconds`, `reason` and `prompt` are required when `stop` is not true"
                        .into(),
                ),
            }
        }
        _ => Err(format!("{name} is not a scheduled task tool")),
    }
}

pub(crate) fn definitions() -> Vec<ToolDefinition> {
    let min_delay = MIN_WAKEUP_DELAY.as_secs();
    let max_delay = MAX_WAKEUP_DELAY.as_secs();
    vec![
        ToolDefinition {
            name: CRON_CREATE_TOOL_NAME.into(),
            description: format!(
                "Schedule a prompt to run in this Chat at a future time: recurring on a cron schedule, or once at a specific time. The task belongs to this Chat; the host runs it between turns.

Uses a standard 5-field cron expression in the user's local time zone: minute hour day-of-month month day-of-week. \"0 9 * * *\" means 9am local; no time zone conversion is needed. Fields take `*`, numbers, ranges `1-5`, steps `*/15` and lists `1,15`; no names, `L`, `W` or `?`.

## One-shot tasks (recurring: false)

For \"remind me at X\" or \"at <time>, do Y\" requests: the task runs once, then deletes itself. Pin minute, hour, day-of-month and month to specific values:
  \"remind me at 2:30pm today to check the deploy\" → cron: \"30 14 <today_dom> <today_month> *\", recurring: false
  \"tomorrow morning, run the smoke test\" → cron: \"57 8 <tomorrow_dom> <tomorrow_month> *\", recurring: false

## Recurring tasks (recurring: true, the default)

For \"every N minutes\", \"every hour\" or \"weekdays at 9am\" requests:
  \"*/5 * * * *\" (every 5 minutes), \"0 * * * *\" (hourly), \"0 9 * * 1-5\" (weekdays at 9am local)

## Avoid the :00 and :30 minute marks when the task allows it

When the user's request is approximate, pick a minute that is not 0 or 30, so requests from many users do not land at the same instant:
  \"every morning around 9\" → \"57 8 * * *\" or \"3 9 * * *\" (not \"0 9 * * *\")
  \"hourly\" → \"7 * * * *\" (not \"0 * * * *\")
Use minute 0 or 30 only when the user names that exact time and clearly means it.

## Runtime behavior

A task runs only between turns: when it comes due while a turn is running, it waits for the turn to end, and times passed meanwhile run once, not once each. Each task gets a small fixed jitter: recurring tasks run late by up to half the gap to their next run and at most 30 minutes, except that \"*/5 * * * *\" runs 15 seconds early; one-shot tasks landing on :00 or :30 run up to 90 seconds early.

Recurring tasks expire {LIFETIME_DAYS} days after creation: they run one final time, then are deleted. Tell the user about the {LIFETIME_DAYS}-day limit when scheduling recurring tasks. A Chat holds at most {max} scheduled tasks.

On a host without permission requests, a task you create waits for the user to confirm it before it starts timing; only the one task the user's /loop schedules, created exactly as its \"This /loop\" section gives it, does not.

Returns the task id, which {CRON_DELETE_TOOL_NAME} takes.",
                max = tickloop_agent_protocol::MAX_SCHEDULED_TASKS_PER_CHAT,
            ),
            input_schema: json!({"type":"object","properties":{
                "cron":{"type":"string","description":"Standard 5-field cron expression in the user's local time zone: \"M H DoM Mon DoW\"."},
                "prompt":{"type":"string","minLength":1,"description":format!("The prompt to run at each scheduled time. Pass `{LOOP_INSTRUCTIONS_PLACEHOLDER}` to run the Chat's loop.md, or the built-in maintenance instructions when there is none.")},
                "recurring":{"type":"boolean","description":"true (default) runs at every time the expression names; false runs once at the next one, then deletes the task."}
            },"required":["cron","prompt"],"additionalProperties":false}),
        },
        ToolDefinition {
            name: CRON_LIST_TOOL_NAME.into(),
            description: format!("List this Chat's scheduled tasks with their ids, schedules, next run times and prompts, including tasks waiting for the user's confirmation and the self-paced /loop task {SCHEDULE_WAKEUP_TOOL_NAME} keeps."),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        },
        ToolDefinition {
            name: CRON_DELETE_TOOL_NAME.into(),
            description: format!("Cancel a scheduled task of this Chat by its id, as {CRON_CREATE_TOOL_NAME} or {CRON_LIST_TOOL_NAME} gave it."),
            input_schema: json!({"type":"object","properties":{
                "id":{"type":"string","description":"The 8-character task id."}
            },"required":["id"],"additionalProperties":false}),
        },
        ToolDefinition {
            name: SCHEDULE_WAKEUP_TOOL_NAME.into(),
            description: format!("Schedule when to resume work in /loop self-paced mode: the user ran /loop without an interval, asking you to pace iterations of a task yourself. Call it before ending the turn to keep the loop alive, or with `stop: true` (and no other field) to end the loop now.
Do not schedule a short wakeup to poll for work the host tracks, such as a background Subagent or Shell: you are notified when it finishes. Schedule a long fallback (1200s or more) instead, so the loop survives if the work hangs. For external state the host cannot track, such as a CI run, a deploy or a remote queue, pick a delay matched to how fast that state changes.
Pass the /loop prompt back via `prompt` each turn; the task keeps the instruction it was started with. For a /loop without a prompt, pass `{LOOP_INSTRUCTIONS_PLACEHOLDER}`.
If a turn ends without calling this tool or stopping, the host wakes the loop once more about 20 minutes later; when that turn does not reschedule either, the loop ends. Esc and /stop cancel the pending wakeup.

## Picking delaySeconds
Match the delay to what you are actually waiting for: when actively polling external state, pick it from how fast that state changes (a CI run that takes about 8 minutes deserves one ~480s check, not eight 60s ones); for idle iterations with no specific signal to watch, default to 1200s-1800s. The host clamps the delay to [{min_delay}, {max_delay}].

## The reason field
One short sentence on what you chose and why, shown to the user: \"watching CI run\" beats \"waiting\"."),
            input_schema: json!({"type":"object","properties":{
                "delaySeconds":{"type":"integer","minimum":0,"description":format!("Seconds from now to wake up; clamped to [{min_delay}, {max_delay}].")},
                "reason":{"type":"string","description":"One short sentence explaining the chosen delay."},
                "prompt":{"type":"string","description":"The /loop prompt to run on wake-up."},
                "stop":{"type":"boolean","description":"End the loop now; omit every other field."}
            },"additionalProperties":false}),
        },
    ]
}

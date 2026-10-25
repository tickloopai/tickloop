//! The root Agent's scheduled task tools, adapted from Claude Code's CronCreate, CronList,
//! CronDelete and ScheduleWakeup.

use std::time::Duration;
use tickloop_harness_protocol::ScheduledTaskIdKind;
use tickloop_ids::ModelRef;

use serde::Deserialize;
use serde_json::{Value, json};
use tickloop_harness_cron::Schedule;
use tickloop_harness_protocol::{MAX_SCHEDULED_TASK_TITLE_CHARS, ToolDefinition};

use super::tasks::{CronTask, TaskRules, duration_words, rate, title};
use crate::host_tools::HostToolPermission;
use tickloop_harness_policy::loop_skill::DEFAULT_INSTRUCTIONS_PROMPT as LOOP_INSTRUCTIONS_PLACEHOLDER;

pub(crate) const CRON_CREATE_TOOL_NAME: &str = "CronCreate";
pub(crate) const CRON_LIST_TOOL_NAME: &str = "CronList";
pub(crate) const CRON_UPDATE_TOOL_NAME: &str = "CronUpdate";
pub(crate) const CRON_DELETE_TOOL_NAME: &str = "CronDelete";
pub(crate) const SCHEDULE_WAKEUP_TOOL_NAME: &str = "ScheduleWakeup";

/// Recurring schedules the CronCreate description gives as examples, where the host allows
/// them.
const RECURRING_EXAMPLES: [(&str, &str); 3] = [
    ("*/5 * * * *", "every 5 minutes"),
    ("0 * * * *", "hourly"),
    ("0 9 * * 1-5", "weekdays at 9am local"),
];

/// What the scheduled task tools promise the model on this host.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ToolTerms {
    pub rules: TaskRules,
    /// Whether a task the Agent creates waits for the user's confirmation on a host without
    /// permission requests.
    pub confirms_agent_tasks: bool,
    /// The user's time zone, which new tasks read their cron expressions in.
    pub zone: chrono_tz::Tz,
}

impl ToolTerms {
    /// Claude Code's, in UTC.
    #[cfg(test)]
    pub(crate) const DEFAULT: Self = Self {
        rules: TaskRules::DEFAULT,
        confirms_agent_tasks: true,
        zone: chrono_tz::Tz::UTC,
    };
}

pub(crate) fn is_scheduled_task_tool(name: &str) -> bool {
    matches!(
        name,
        CRON_CREATE_TOOL_NAME
            | CRON_LIST_TOOL_NAME
            | CRON_UPDATE_TOOL_NAME
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
        title: Option<String>,
    },
    List,
    Update {
        id: ModelRef<ScheduledTaskIdKind>,
        cron: Option<String>,
        prompt: Option<String>,
        recurring: Option<bool>,
        title: Option<String>,
    },
    Delete {
        id: ModelRef<ScheduledTaskIdKind>,
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
    title: Option<String>,
}

fn recurring_by_default() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListInput {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateInput {
    id: String,
    cron: Option<String>,
    prompt: Option<String>,
    recurring: Option<bool>,
    title: Option<String>,
}

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
            let title = input
                .title
                .as_deref()
                .map(title)
                .transpose()
                .map_err(|error| error.to_string())?;
            Ok(ScheduledTaskCall::Create {
                cron: input.cron,
                prompt: input.prompt,
                recurring: input.recurring,
                title,
            })
        }
        CRON_LIST_TOOL_NAME => {
            serde_json::from_value::<ListInput>(input.clone()).map_err(invalid)?;
            Ok(ScheduledTaskCall::List)
        }
        CRON_UPDATE_TOOL_NAME => {
            let input: UpdateInput = serde_json::from_value(input.clone()).map_err(invalid)?;
            if input.cron.is_none()
                && input.prompt.is_none()
                && input.recurring.is_none()
                && input.title.is_none()
            {
                return Err(format!(
                    "{name} needs at least one of `cron`, `prompt`, `recurring` and `title`"
                ));
            }
            if input
                .prompt
                .as_ref()
                .is_some_and(|prompt| prompt.trim().is_empty())
            {
                return Err(format!("{name} needs a prompt to run"));
            }
            let title = input
                .title
                .as_deref()
                .map(title)
                .transpose()
                .map_err(|error| error.to_string())?;
            Ok(ScheduledTaskCall::Update {
                id: ModelRef::new(input.id),
                cron: input.cron,
                prompt: input.prompt,
                recurring: input.recurring,
                title,
            })
        }
        CRON_DELETE_TOOL_NAME => {
            let input: DeleteInput = serde_json::from_value(input.clone()).map_err(invalid)?;
            Ok(ScheduledTaskCall::Delete {
                id: ModelRef::new(input.id),
            })
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

/// The tool definitions on `terms`, ScheduleWakeup only with `schedule_wakeup`.
pub(crate) fn definitions(terms: ToolTerms, schedule_wakeup: bool) -> Vec<ToolDefinition> {
    let (min_delay, max_delay) = terms.rules.wakeup_delays();
    let (min_delay, max_delay) = (min_delay.as_secs(), max_delay.as_secs());
    let recurring_examples = RECURRING_EXAMPLES
        .iter()
        .filter(|(cron, _)| {
            Schedule::parse(cron).is_ok_and(|schedule| {
                terms
                    .rules
                    .check_interval(&CronTask::Recurring(schedule))
                    .is_ok()
            })
        })
        .map(|(cron, meaning)| format!("\"{cron}\" ({meaning})"))
        .collect::<Vec<_>>()
        .join(", ");
    let frequency_limit = terms.rules.min_interval.map_or(String::new(), |min_interval| {
        format!(
            "\n\nOn this host a recurring task runs at most {}: any two consecutive times its expression names must be at least that far apart, and {CRON_CREATE_TOOL_NAME} and {CRON_UPDATE_TOOL_NAME} refuse a schedule that runs more often. One-shot tasks are not limited.",
            rate(min_interval)
        )
    });
    let lifetime = match terms.rules.lifetime {
        Some(lifetime) => {
            let lifetime = duration_words(lifetime);
            format!(
                "Recurring tasks expire {lifetime} after creation: they run one final time, then are deleted. Tell the user about this limit when scheduling recurring tasks."
            )
        }
        None => "Recurring tasks run until they are deleted.".to_owned(),
    };
    let confirmation_on_change = if terms.confirms_agent_tasks {
        " On a host without permission requests, a task you change waits for the user to confirm it again before it times again, and a task waiting for confirmation cannot be changed: the user confirms or rejects it first."
    } else {
        ""
    };
    let confirmation = if terms.confirms_agent_tasks {
        "\n\nOn a host without permission requests, a task you create waits for the user to confirm it before it starts timing; only the one task the user's /loop schedules, created exactly as its \"This /loop\" section gives it, does not."
    } else {
        ""
    };
    let mut definitions = vec![
        ToolDefinition {
            name: CRON_CREATE_TOOL_NAME.into(),
            description: format!(
                "Schedule a prompt to run in this Chat at a future time: recurring on a cron schedule, or once at a specific time. The task belongs to this Chat; the host runs it between turns.

Uses a standard 5-field cron expression in the user's local time zone, {zone}: minute hour day-of-month month day-of-week. \"0 9 * * *\" means 9am local; no time zone conversion is needed. Fields take `*`, numbers, ranges `1-5`, steps `*/15` and lists `1,15`; no names, `L`, `W` or `?`.

## One-shot tasks (recurring: false)

For \"remind me at X\" or \"at <time>, do Y\" requests: the task runs once, then deletes itself. Pin minute, hour, day-of-month and month to specific values:
  \"remind me at 2:30pm today to check the deploy\" → cron: \"30 14 <today_dom> <today_month> *\", recurring: false
  \"tomorrow morning, run the smoke test\" → cron: \"57 8 <tomorrow_dom> <tomorrow_month> *\", recurring: false

## Recurring tasks (recurring: true, the default)

For \"every N minutes\", \"every hour\" or \"weekdays at 9am\" requests:
  {recurring_examples}{frequency_limit}

## Avoid the :00 and :30 minute marks when the task allows it

When the user's request is approximate, pick a minute that is not 0 or 30, so requests from many users do not land at the same instant:
  \"every morning around 9\" → \"57 8 * * *\" or \"3 9 * * *\" (not \"0 9 * * *\")
  \"hourly\" → \"7 * * * *\" (not \"0 * * * *\")
Use minute 0 or 30 only when the user names that exact time and clearly means it.

## Runtime behavior

A task runs only between turns: when it comes due while a turn is running, it waits for the turn to end, and times passed meanwhile run once, not once each. Each task gets a small fixed jitter: recurring tasks run late by up to half the gap to their next run and at most 30 minutes, except that \"*/5 * * * *\" runs 15 seconds early; one-shot tasks landing on :00 or :30 run up to 90 seconds early.

{lifetime} A Chat holds at most {max} scheduled tasks.{confirmation}

To change a task that exists, such as its time or its prompt, use {CRON_UPDATE_TOOL_NAME}; do not delete it and create a new one.

Returns the task id, which {CRON_UPDATE_TOOL_NAME} and {CRON_DELETE_TOOL_NAME} take.",
                max = tickloop_harness_protocol::MAX_SCHEDULED_TASKS_PER_CHAT,
                zone = terms.zone.name(),
            ),
            input_schema: json!({"type":"object","properties":{
                "cron":{"type":"string","description":"Standard 5-field cron expression in the user's local time zone: \"M H DoM Mon DoW\"."},
                "prompt":{"type":"string","minLength":1,"description":format!("The prompt to run at each scheduled time. Pass `{LOOP_INSTRUCTIONS_PLACEHOLDER}` to run the Chat's loop.md, or the built-in maintenance instructions when there is none.")},
                "recurring":{"type":"boolean","description":"true (default) runs at every time the expression names; false runs once at the next one, then deletes the task."},
                "title":{"type":"string","minLength":1,"description":format!("A short name the user sees for the task, such as \"Daily news digest\", at most {MAX_SCHEDULED_TASK_TITLE_CHARS} characters.")}
            },"required":["cron","prompt"],"additionalProperties":false}),
        },
        ToolDefinition {
            name: CRON_LIST_TOOL_NAME.into(),
            description: format!("List this Chat's scheduled tasks with their ids, titles, schedules, time zones, next run times, prompts and paused status (only the user can pause or resume tasks), including tasks waiting for the user's confirmation and the self-paced /loop task {SCHEDULE_WAKEUP_TOOL_NAME} keeps. Each task keeps the time zone it was created in; tasks created now use the user's, {zone}.", zone = terms.zone.name()),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        },
        ToolDefinition {
            name: CRON_UPDATE_TOOL_NAME.into(),
            description: format!(
                "Change a scheduled task of this Chat in place, by its id as {CRON_CREATE_TOOL_NAME} or {CRON_LIST_TOOL_NAME} gave it: its schedule, whether it recurs, its prompt or its title. Use it whenever the user wants a task changed, such as \"move it to 9:20\"; do not delete the task and create a new one, which would give it a new id and break the user's links to it.

Give the id and at least one field; fields left out stay as they are. A new `cron` or `recurring` is read in the user's local time zone, {zone}, and the next run is timed from now; the run already queued for the old time is withdrawn. The task keeps its id, its end and its lifetime rules.{frequency_limit}{confirmation_on_change} The self-paced /loop task is rescheduled with {SCHEDULE_WAKEUP_TOOL_NAME} instead.

Returns the changed task, as {CRON_CREATE_TOOL_NAME} returns a new one.",
                zone = terms.zone.name(),
            ),
            input_schema: json!({"type":"object","properties":{
                "id":{"type":"string","description":format!("The task id {CRON_CREATE_TOOL_NAME} or {CRON_LIST_TOOL_NAME} returned.")},
                "cron":{"type":"string","description":"A new standard 5-field cron expression in the user's local time zone: \"M H DoM Mon DoW\"."},
                "prompt":{"type":"string","minLength":1,"description":"A new prompt to run at each scheduled time."},
                "recurring":{"type":"boolean","description":"true runs at every time the expression names; false runs once at the next one, then deletes the task."},
                "title":{"type":"string","minLength":1,"description":format!("A new short name the user sees for the task, at most {MAX_SCHEDULED_TASK_TITLE_CHARS} characters.")}
            },"required":["id"],"additionalProperties":false}),
        },
        ToolDefinition {
            name: CRON_DELETE_TOOL_NAME.into(),
            description: format!("Cancel a scheduled task of this Chat by its id, as {CRON_CREATE_TOOL_NAME} or {CRON_LIST_TOOL_NAME} gave it."),
            input_schema: json!({"type":"object","properties":{
                "id":{"type":"string","description":format!("The task id {CRON_CREATE_TOOL_NAME} or {CRON_LIST_TOOL_NAME} returned.")}
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
    ];
    if !schedule_wakeup {
        definitions.retain(|definition| definition.name != SCHEDULE_WAKEUP_TOOL_NAME);
    }
    definitions
}

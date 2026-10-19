use super::execution::ConsoleExecution as Agent;
use uuid::Uuid;

use super::{current_time_ms, safe_text};
use tickloop_agent_protocol::{Recurrence, Trigger, TriggerPromptSource};

pub(super) const SYNTAX: &str = "use /loops [GOAL_ID|new|trigger [GOAL_ID]|schedule GOAL_ID SECONDS|RULE [--for DURATION_SECONDS] PROMPT]";
/// The most words a schedule rule takes, as in `weekly mon,fri 09:00 +08:00`.
const MAX_RULE_WORDS: usize = 4;

#[derive(Debug, PartialEq)]
pub(super) enum LoopCommand<'a> {
    List,
    New,
    Select(Uuid),
    Trigger(Option<Uuid>),
    Schedule { goal: Uuid, arguments: &'a str },
}

impl<'a> LoopCommand<'a> {
    pub(super) fn parse(command: &'a str) -> Result<Self, &'static str> {
        let mut fields = command.splitn(2, char::is_whitespace);
        if fields.next() != Some("/loops") {
            return Err(SYNTAX);
        }
        let arguments = fields.next().unwrap_or_default().trim();
        match arguments {
            "" => Ok(Self::List),
            "new" => Ok(Self::New),
            "trigger" => Ok(Self::Trigger(None)),
            _ => {
                let (action, rest) = arguments
                    .split_once(char::is_whitespace)
                    .unwrap_or((arguments, ""));
                match action {
                    "trigger" => rest
                        .trim()
                        .parse()
                        .map(|goal| Self::Trigger(Some(goal)))
                        .map_err(|_| SYNTAX),
                    "schedule" => {
                        let (goal, arguments) =
                            rest.trim().split_once(char::is_whitespace).ok_or(SYNTAX)?;
                        Ok(Self::Schedule {
                            goal: goal.parse().map_err(|_| "goal ID must be a UUID")?,
                            arguments: arguments.trim(),
                        })
                    }
                    _ => arguments.parse().map(Self::Select).map_err(|_| SYNTAX),
                }
            }
        }
    }
}

/// The schedule that starts `arguments` and the rest after it: a number of seconds, or a rule
/// such as `daily 09:00 +08:00`, the longest that reads as one.
fn parse_schedule(arguments: &str) -> Result<(Recurrence, &str), &'static str> {
    let invalid = "interval must be a positive number of seconds or a rule such as \
                   `every 5m`, `daily 09:00 +08:00` or `weekly mon,fri 09:00`";
    let (first, rest) = arguments.split_once(char::is_whitespace).ok_or(SYNTAX)?;
    if first.bytes().all(|byte| byte.is_ascii_digit()) {
        let schedule = Recurrence::parse(&format!("every {first}s"))
            .map_err(|_| "interval must be a positive number of seconds")?;
        return Ok((schedule, rest));
    }
    let word_ends = arguments
        .char_indices()
        .filter(|(index, character)| {
            character.is_whitespace()
                && arguments[..*index]
                    .chars()
                    .last()
                    .is_some_and(|previous| !previous.is_whitespace())
        })
        .map(|(index, _)| index)
        .take(MAX_RULE_WORDS)
        .collect::<Vec<_>>();
    word_ends
        .iter()
        .rev()
        .find_map(|end| {
            Recurrence::parse(&arguments[..*end])
                .ok()
                .map(|schedule| (schedule, &arguments[*end..]))
        })
        .ok_or(invalid)
}

#[derive(Debug, PartialEq)]
pub(super) struct TimerSchedule {
    schedule: Recurrence,
    duration_seconds: Option<u64>,
    prompt: String,
}

impl TimerSchedule {
    pub(super) fn parse(arguments: &str) -> Result<Self, &'static str> {
        let (schedule, rest) = parse_schedule(arguments.trim())?;
        let rest = rest.trim();
        let (duration_seconds, prompt) = if rest.split_whitespace().next() == Some("--for") {
            let (duration, prompt) = rest[5..]
                .trim_start()
                .split_once(char::is_whitespace)
                .ok_or("--for requires duration seconds and a prompt")?;
            let duration = duration
                .parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or("duration must be a positive number of seconds")?;
            (Some(duration), prompt.trim())
        } else {
            (None, rest)
        };
        if prompt.is_empty() {
            return Err("a timer prompt is required");
        }
        let timer = Self {
            schedule,
            duration_seconds,
            prompt: prompt.into(),
        };
        timer.expires_at_ms()?;
        Ok(timer)
    }

    fn expires_at_ms(&self) -> Result<Option<u64>, &'static str> {
        self.duration_seconds
            .map(|seconds| {
                seconds
                    .checked_mul(1000)
                    .and_then(|duration| current_time_ms().checked_add(duration))
                    .ok_or("duration is too large")
            })
            .transpose()
    }

    pub(super) async fn apply(
        self,
        agent: &Agent,
        chat: Uuid,
        goal: Uuid,
    ) -> Result<Trigger, String> {
        let snapshot = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        let selected = snapshot
            .loops
            .iter()
            .find(|item| item.id == goal)
            .ok_or("Goal does not belong to the current Chat; use /loops.")?;
        let trigger = self.prepare(selected.trigger.clone())?;
        agent
            .configure_trigger(chat, goal, Some(trigger.clone()))
            .await
            .map_err(|error| error.to_string())?;
        Ok(trigger)
    }

    pub(super) fn prepare(self, current: Option<Trigger>) -> Result<Trigger, String> {
        let mut trigger = current.unwrap_or(Trigger {
            id: Uuid::new_v4(),
            prompt: String::new(),
            prompt_source: TriggerPromptSource::Explicit,
            schedule: None,
            armed_at_ms: None,
            last_fired_at_ms: None,
            confirmation: None,
            expires_at_ms: None,
            accepts_events: false,
            armed: true,
            subscriptions: Vec::new(),
        });
        trigger.id = Uuid::new_v4();
        trigger.armed = true;
        trigger.schedule = Some(self.schedule);
        trigger.expires_at_ms = self.expires_at_ms()?;
        trigger.set_explicit_prompt(self.prompt);
        Ok(trigger)
    }
}

/// How a Trigger's schedule reads in the Loop's details.
pub(super) fn schedule_line(schedule: &Recurrence) -> String {
    match schedule.interval() {
        Some(interval) => format!(
            "Interval: {} seconds while this Agent runs.",
            interval.as_secs()
        ),
        None => format!("Schedule: {schedule} while this Agent runs."),
    }
}

pub(super) fn trigger_lines(trigger: Option<&Trigger>) -> Vec<String> {
    let Some(trigger) = trigger else {
        return vec!["No trigger configured.".into()];
    };
    vec![
        format!(
            "Trigger: {} · armed {} · explicit events {}",
            trigger.id, trigger.armed, trigger.accepts_events
        ),
        trigger
            .schedule
            .map_or("Timer: off".into(), |schedule| schedule_line(&schedule)),
        trigger
            .expires_at_ms
            .map_or("Deadline: none".into(), |expiry| {
                format!(
                    "Expires in {} seconds.",
                    expiry.saturating_sub(current_time_ms()) / 1000
                )
            }),
        format!("Tick Subscriptions: {}", trigger.subscriptions.len()),
        match trigger.prompt_source {
            TriggerPromptSource::Explicit => format!("Prompt: {}", safe_text(&trigger.prompt)),
            TriggerPromptSource::LoopGoal => format!(
                "Prompt: the Loop goal, delivered as background: {}",
                safe_text(&trigger.prompt)
            ),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_actions_require_unambiguous_arguments() {
        let goal = Uuid::new_v4();
        assert_eq!(LoopCommand::parse("/loops"), Ok(LoopCommand::List));
        assert_eq!(LoopCommand::parse("/loops new"), Ok(LoopCommand::New));
        assert_eq!(
            LoopCommand::parse(&format!("/loops {goal}")),
            Ok(LoopCommand::Select(goal))
        );
        assert_eq!(
            LoopCommand::parse(&format!("/loops trigger {goal}")),
            Ok(LoopCommand::Trigger(Some(goal)))
        );
        assert_eq!(
            LoopCommand::parse(&format!("/loops schedule {goal} 60 check")),
            Ok(LoopCommand::Schedule {
                goal,
                arguments: "60 check"
            })
        );
        for invalid in [
            "/goal",
            "/loops new extra",
            "/loops trigger invalid",
            "/loops schedule 60 check",
            "/loops unknown",
            "/loopsnew",
        ] {
            assert!(LoopCommand::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn timer_requires_positive_bounded_values_and_preserves_the_prompt() {
        assert_eq!(
            TimerSchedule::parse("60 --for 3600 check the price"),
            Ok(TimerSchedule {
                schedule: Recurrence::parse("every 1m").unwrap(),
                duration_seconds: Some(3600),
                prompt: "check the price".into()
            })
        );
        assert_eq!(
            TimerSchedule::parse(" 60\tcheck\nprices ").unwrap().prompt,
            "check\nprices"
        );
        let daily = TimerSchedule::parse("daily 09:00 +08:00 --for 60 check the price").unwrap();
        assert_eq!(daily.schedule.to_string(), "daily 09:00 +08:00");
        assert_eq!(daily.duration_seconds, Some(60));
        assert_eq!(daily.prompt, "check the price");
        assert_eq!(
            TimerSchedule::parse("weekly mon,fri 09:00 check")
                .unwrap()
                .schedule
                .to_string(),
            "weekly mon,fri 09:00"
        );
        for invalid in [
            "daily check",
            "every 0s check",
            "0 check",
            "-1 check",
            "60",
            "60 ",
            "60 --for 0 check",
            "60 --for 1",
            "60 --for 18446744073709551615 check",
        ] {
            assert!(TimerSchedule::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn scheduling_over_a_goal_fallback_trigger_makes_its_prompt_explicit() {
        let fallback = Trigger::for_loop_goal("watch 700.HK");
        assert_eq!(
            trigger_lines(Some(&fallback)).last().unwrap(),
            "Prompt: the Loop goal, delivered as background: watch 700.HK"
        );
        let scheduled = TimerSchedule::parse("60 check the price")
            .unwrap()
            .prepare(Some(fallback))
            .unwrap();
        assert_eq!(scheduled.prompt, "check the price");
        assert_eq!(scheduled.prompt_source, TriggerPromptSource::Explicit);
        assert_eq!(
            trigger_lines(Some(&scheduled)).last().unwrap(),
            "Prompt: check the price"
        );
    }
}

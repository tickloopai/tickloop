use tickloop_agent::runtime::{Agent, Trigger};
use uuid::Uuid;

use super::{current_time_ms, safe_text};

pub(super) const SYNTAX: &str = "use /loops [GOAL_ID|new|trigger [GOAL_ID]|schedule GOAL_ID SECONDS [--for DURATION_SECONDS] PROMPT]";

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

#[derive(Debug, PartialEq)]
pub(super) struct TimerSchedule {
    interval_seconds: u64,
    duration_seconds: Option<u64>,
    prompt: String,
}

impl TimerSchedule {
    pub(super) fn parse(arguments: &str) -> Result<Self, &'static str> {
        let (seconds, rest) = arguments
            .trim()
            .split_once(char::is_whitespace)
            .ok_or(SYNTAX)?;
        let interval_seconds = seconds
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or("interval must be a positive number of seconds")?;
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
        let schedule = Self {
            interval_seconds,
            duration_seconds,
            prompt: prompt.into(),
        };
        schedule.expires_at_ms()?;
        Ok(schedule)
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
        let mut trigger = selected.trigger.clone().unwrap_or(Trigger {
            id: Uuid::new_v4(),
            prompt: String::new(),
            interval_seconds: None,
            expires_at_ms: None,
            accepts_events: false,
            armed: true,
            subscriptions: Vec::new(),
        });
        trigger.id = Uuid::new_v4();
        trigger.armed = true;
        trigger.interval_seconds = Some(self.interval_seconds);
        trigger.expires_at_ms = self.expires_at_ms()?;
        trigger.prompt = self.prompt;
        agent
            .configure_trigger(chat, goal, Some(trigger.clone()))
            .await
            .map_err(|error| error.to_string())?;
        Ok(trigger)
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
            .interval_seconds
            .map_or("Timer: off".into(), |seconds| {
                format!("Interval: {seconds} seconds while this Agent runs.")
            }),
        trigger
            .expires_at_ms
            .map_or("Deadline: none".into(), |expiry| {
                format!(
                    "Expires in {} seconds.",
                    expiry.saturating_sub(current_time_ms()) / 1000
                )
            }),
        format!("Tick Subscriptions: {}", trigger.subscriptions.len()),
        format!("Prompt: {}", safe_text(&trigger.prompt)),
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
                interval_seconds: 60,
                duration_seconds: Some(3600),
                prompt: "check the price".into()
            })
        );
        assert_eq!(
            TimerSchedule::parse(" 60\tcheck\nprices ").unwrap().prompt,
            "check\nprices"
        );
        for invalid in [
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
}

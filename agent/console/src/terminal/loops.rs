use uuid::Uuid;

use super::safe_text;
use tickloop_agent_protocol::{Trigger, TriggerPromptSource};

pub(super) const SYNTAX: &str = "use /loops [GOAL_ID|new|trigger [GOAL_ID]]";

#[derive(Debug, PartialEq)]
pub(super) enum LoopCommand {
    List,
    New,
    Select(Uuid),
    Trigger(Option<Uuid>),
}

impl LoopCommand {
    pub(super) fn parse(command: &str) -> Result<Self, &'static str> {
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
                    _ => arguments.parse().map(Self::Select).map_err(|_| SYNTAX),
                }
            }
        }
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
        for invalid in [
            "/goal",
            "/loops new extra",
            "/loops trigger invalid",
            &format!("/loops schedule {goal} 60 check"),
            "/loops unknown",
            "/loopsnew",
        ] {
            assert!(LoopCommand::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn a_goal_fallback_trigger_shows_its_prompt_as_background() {
        let fallback = Trigger::for_loop_goal("watch 700.HK");
        assert_eq!(
            trigger_lines(Some(&fallback)).last().unwrap(),
            "Prompt: the Loop goal, delivered as background: watch 700.HK"
        );
        let mut explicit = fallback;
        explicit.set_explicit_prompt("check the price".into());
        assert_eq!(
            trigger_lines(Some(&explicit)).last().unwrap(),
            "Prompt: check the price"
        );
    }
}

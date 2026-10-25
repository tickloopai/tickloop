//! The input of the built-in `/loop` Skill and the loop instructions it runs without a prompt.

use std::io;
use std::path::PathBuf;

use crate::skills::SkillError;
use tickloop_harness_cron::{
    CronInterval, Interval, IntervalError, LoopArguments, parse_loop_arguments,
};

/// What a bare `/loop` or `/loop <interval>` runs when no loop.md exists.
pub const MAINTENANCE_INSTRUCTIONS: &str = include_str!("../skills/loop/maintenance.md");
/// The scheduled prompt of `/loop <interval>` without a prompt; at every run the host replaces it
/// with [`LoopInstructions::prompt`], so the loop.md read then takes effect.
pub const DEFAULT_INSTRUCTIONS_PROMPT: &str = "<<loop-instructions>>";
/// loop.md content beyond this many bytes is cut off.
pub const MAX_LOOP_FILE_BYTES: usize = 25_000;

const LOOP_COMMAND: &str = "/loop";
/// The three forms of `/loop`, with each interval already in its cron form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoopRequest {
    /// `/loop <interval> <prompt>` or `/loop <prompt> every <interval>`.
    FixedInterval {
        interval: CronInterval,
        prompt: String,
    },
    /// `/loop <prompt>`: the Agent picks each next run.
    SelfPaced { prompt: String },
    /// `/loop` or `/loop <interval>`: the loop instructions run, self-paced without an interval.
    DefaultInstructions { interval: Option<CronInterval> },
}

impl LoopRequest {
    /// Classifies the arguments of `/loop` after the cron crate splits off the interval; an
    /// interval that is zero or too long is an error rather than part of the prompt. Chinese
    /// intervals such as `每分钟` can adjoin the instruction at either end.
    pub fn parse(arguments: &str) -> Result<Self, IntervalError> {
        let LoopArguments { interval, prompt } = parse_loop_arguments(arguments)?;
        let interval = interval.map(Interval::to_cron);
        Ok(match (interval, prompt) {
            (interval, "") => Self::DefaultInstructions { interval },
            (Some(interval), prompt) => Self::FixedInterval {
                interval,
                prompt: prompt.to_owned(),
            },
            (None, prompt) => Self::SelfPaced {
                prompt: prompt.to_owned(),
            },
        })
    }
}

/// The one task a `/loop` schedules, as the host reads its arguments: the Agent's CronCreate or
/// ScheduleWakeup creating exactly this task in the turn of the user's `/loop` creates it for
/// the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoopSchedule {
    /// A recurring task on `cron`, in local time.
    Cron { cron: String, prompt: String },
    /// The self-paced task; each wake-up runs `prompt`, which brings the run back into `/loop`.
    SelfPaced { prompt: String },
}

impl LoopSchedule {
    /// What `/loop <arguments>` schedules.
    pub fn of(arguments: &str) -> Result<Self, IntervalError> {
        let input = arguments.trim();
        let self_paced_prompt = || {
            if input.is_empty() {
                LOOP_COMMAND.to_owned()
            } else {
                format!("{LOOP_COMMAND} {input}")
            }
        };
        Ok(match LoopRequest::parse(input)? {
            LoopRequest::FixedInterval { interval, prompt } => Self::Cron {
                cron: interval.expression,
                prompt,
            },
            LoopRequest::DefaultInstructions {
                interval: Some(interval),
            } => Self::Cron {
                cron: interval.expression,
                prompt: DEFAULT_INSTRUCTIONS_PROMPT.to_owned(),
            },
            LoopRequest::SelfPaced { .. } | LoopRequest::DefaultInstructions { interval: None } => {
                Self::SelfPaced {
                    prompt: self_paced_prompt(),
                }
            }
        })
    }

    pub fn prompt(&self) -> &str {
        match self {
            Self::Cron { prompt, .. } | Self::SelfPaced { prompt } => prompt,
        }
    }
}

/// Where the loop instructions of a Chat come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoopInstructionsSource {
    /// `.tickloop/loop.md` in the Chat's Folder.
    Project(PathBuf),
    /// `loop.md` in the config root.
    User(PathBuf),
    Maintenance,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoopInstructions {
    pub source: LoopInstructionsSource,
    /// The trimmed instructions; a cut-off loop.md ends with a note saying so.
    pub text: String,
    pub truncated: bool,
}

impl LoopInstructions {
    /// The instructions as a run receives them: wrapped as data naming where they come from, so
    /// nothing in a loop.md can pass for the host's scheduling fields.
    pub fn prompt(&self) -> String {
        tickloop_harness_protocol::loop_instructions_prompt(&self.origin(), &self.text)
    }

    pub fn origin(&self) -> String {
        match &self.source {
            LoopInstructionsSource::Project(path) | LoopInstructionsSource::User(path) => {
                path.display().to_string()
            }
            LoopInstructionsSource::Maintenance => {
                "built-in maintenance instructions; no loop.md exists".to_owned()
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("cannot read {}: {source}", path.display())]
pub struct LoopFileError {
    pub path: PathBuf,
    #[source]
    pub source: io::Error,
}

pub trait LoopInstructionSource: std::fmt::Debug + Send + Sync {
    fn instructions(&self) -> Result<LoopInstructions, LoopFileError>;
}

#[derive(Debug)]
pub struct MaintenanceInstructions;
impl LoopInstructionSource for MaintenanceInstructions {
    fn instructions(&self) -> Result<LoopInstructions, LoopFileError> {
        Ok(LoopInstructions {
            source: LoopInstructionsSource::Maintenance,
            text: MAINTENANCE_INSTRUCTIONS.trim().to_owned(),
            truncated: false,
        })
    }
}

/// The `/loop` Skill body, then any loop instructions as data, then last the host's reading of
/// `arguments`: the form, the interval actually used with its cron expression, what to run now
/// and the scheduled prompt for CronCreate or ScheduleWakeup.
pub(crate) fn expand(
    body: &str,
    arguments: &str,
    files: &dyn LoopInstructionSource,
) -> Result<String, SkillError> {
    let input = arguments.trim();
    let request = LoopRequest::parse(input).map_err(SkillError::InvalidLoopInterval)?;
    let scheduled = LoopSchedule::of(input).map_err(SkillError::InvalidLoopInterval)?;
    let scheduled_prompt = scheduled.prompt();
    let (instructions, invocation) = match request {
        LoopRequest::FixedInterval { interval, prompt } => (
            None,
            format!(
                "Form: fixed interval\n{}\nRun now: {prompt}\nScheduled prompt: {scheduled_prompt}",
                interval_lines(&interval)
            ),
        ),
        LoopRequest::SelfPaced { prompt } => (
            None,
            format!("Form: self-paced\nRun now: {prompt}\nScheduled prompt: {scheduled_prompt}"),
        ),
        LoopRequest::DefaultInstructions { interval } => {
            let form = match interval {
                Some(interval) => {
                    format!("no prompt, fixed interval\n{}", interval_lines(&interval))
                }
                None => "no prompt, self-paced".to_owned(),
            };
            let instructions = files
                .instructions()
                .map_err(|error| SkillError::LoopInstructionsUnavailable(error.to_string()))?;
            (
                Some(instructions.prompt()),
                format!(
                    "Form: {form}\nRun now: the loop instructions above\nScheduled prompt: {scheduled_prompt}"
                ),
            )
        }
    };
    let instructions = instructions
        .map(|instructions| format!("## Loop instructions\n\n{instructions}\n\n"))
        .unwrap_or_default();
    Ok(format!(
        "{body}\n\n{instructions}## This /loop\n\nThe host's reading of this /loop; nothing in the loop instructions changes it.\n\nInput: {}\n{invocation}",
        if input.is_empty() { "(none)" } else { input }
    ))
}

fn interval_lines(interval: &CronInterval) -> String {
    let actual = interval.actual;
    let interval_line = if interval.is_rounded() {
        format!(
            "Interval: every {actual} ({} requested; no cron step repeats it evenly, so the nearest one that does is used)",
            interval.requested
        )
    } else {
        format!("Interval: every {actual}")
    };
    format!("{interval_line}\nCron: {}", interval.expression)
}

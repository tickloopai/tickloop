//! The input of the built-in `/loop` Skill and the loop instructions it runs without a prompt.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// What a bare `/loop` or `/loop <interval>` runs when no loop.md exists.
pub const MAINTENANCE_INSTRUCTIONS: &str = include_str!("../skills/loop/maintenance.md");
/// The scheduled prompt of `/loop <interval>` without a prompt; at every run the host replaces it
/// with [`LoopInstructions::prompt`], so the loop.md read then takes effect.
pub const DEFAULT_INSTRUCTIONS_PROMPT: &str = "<<loop-instructions>>";
/// loop.md content beyond this many bytes is cut off.
pub const MAX_LOOP_FILE_BYTES: usize = 25_000;

const LOOP_COMMAND: &str = "/loop";
const LOOP_FILE_NAME: &str = "loop.md";
const PROJECT_LOOP_FILE: &str = ".tickloop/loop.md";
const EVERY: &str = "every";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntervalUnit {
    Seconds,
    Minutes,
    Hours,
    Days,
}

impl IntervalUnit {
    fn from_suffix(suffix: char) -> Option<Self> {
        match suffix {
            's' => Some(Self::Seconds),
            'm' => Some(Self::Minutes),
            'h' => Some(Self::Hours),
            'd' => Some(Self::Days),
            _ => None,
        }
    }

    fn from_word(word: &str) -> Option<Self> {
        match word.to_ascii_lowercase().as_str() {
            "s" | "sec" | "secs" | "second" | "seconds" => Some(Self::Seconds),
            "m" | "min" | "mins" | "minute" | "minutes" => Some(Self::Minutes),
            "h" | "hr" | "hrs" | "hour" | "hours" => Some(Self::Hours),
            "d" | "day" | "days" => Some(Self::Days),
            _ => None,
        }
    }

    fn suffix(self) -> char {
        match self {
            Self::Seconds => 's',
            Self::Minutes => 'm',
            Self::Hours => 'h',
            Self::Days => 'd',
        }
    }

    fn seconds(self) -> u64 {
        match self {
            Self::Seconds => 1,
            Self::Minutes => 60,
            Self::Hours => 60 * 60,
            Self::Days => 24 * 60 * 60,
        }
    }
}

/// An interval as the user wrote it, such as `30m` or `every 2 hours`; it is never zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopInterval {
    pub amount: u32,
    pub unit: IntervalUnit,
}

impl LoopInterval {
    pub fn seconds(self) -> u64 {
        u64::from(self.amount) * self.unit.seconds()
    }

    /// A bare token such as `30m`.
    fn from_token(token: &str) -> Option<Self> {
        let suffix = token.chars().last()?;
        let digits = &token[..token.len() - suffix.len_utf8()];
        Self::new(digits, IntervalUnit::from_suffix(suffix)?)
    }

    fn new(digits: &str, unit: IntervalUnit) -> Option<Self> {
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let amount = digits.parse().ok().filter(|amount| *amount > 0)?;
        Some(Self { amount, unit })
    }
}

impl fmt::Display for LoopInterval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}{}", self.amount, self.unit.suffix())
    }
}

/// The three forms of `/loop`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoopRequest {
    /// `/loop <interval> <prompt>` or `/loop <prompt> every <interval>`.
    FixedInterval {
        interval: LoopInterval,
        prompt: String,
    },
    /// `/loop <prompt>`: the Agent picks each next run.
    SelfPaced { prompt: String },
    /// `/loop` or `/loop <interval>`: the loop instructions run, self-paced without an interval.
    DefaultInstructions { interval: Option<LoopInterval> },
}

/// Classifies the arguments of `/loop`. A leading bare interval (`30m`) wins over a trailing
/// `every` clause (`every 20m`, `every 2 hours`); `every` followed by anything but an interval,
/// as in `check every PR`, stays part of the prompt.
pub fn parse_loop_arguments(arguments: &str) -> LoopRequest {
    let input = arguments.trim();
    let (first_word, after_first_word) =
        input.split_once(char::is_whitespace).unwrap_or((input, ""));
    let parsed = LoopInterval::from_token(first_word)
        .map(|interval| (interval, after_first_word.trim()))
        .or_else(|| trailing_interval(input).map(|(prompt, interval)| (interval, prompt)));
    match parsed {
        Some((interval, "")) => LoopRequest::DefaultInstructions {
            interval: Some(interval),
        },
        Some((interval, prompt)) => LoopRequest::FixedInterval {
            interval,
            prompt: prompt.to_owned(),
        },
        None if input.is_empty() => LoopRequest::DefaultInstructions { interval: None },
        None => LoopRequest::SelfPaced {
            prompt: input.to_owned(),
        },
    }
}

/// `every 20m` or `every 5 minutes` at the end of `input`, with the text before it.
fn trailing_interval(input: &str) -> Option<(&str, LoopInterval)> {
    let (before_last, last) = split_last_word(input);
    let (before_second, second) = split_last_word(before_last);
    if second.eq_ignore_ascii_case(EVERY) {
        return LoopInterval::from_token(last).map(|interval| (before_second, interval));
    }
    let (before_third, third) = split_last_word(before_second);
    if third.eq_ignore_ascii_case(EVERY) {
        let interval = LoopInterval::new(second, IntervalUnit::from_word(last)?)?;
        return Some((before_third, interval));
    }
    None
}

fn split_last_word(text: &str) -> (&str, &str) {
    let text = text.trim_end();
    text.rsplit_once(char::is_whitespace)
        .map_or(("", text), |(before, word)| (before.trim_end(), word))
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
    /// The instructions as a run receives them, naming where they come from.
    pub fn prompt(&self) -> String {
        let origin = match &self.source {
            LoopInstructionsSource::Project(path) | LoopInstructionsSource::User(path) => {
                format!("from {}", path.display())
            }
            LoopInstructionsSource::Maintenance => {
                "built-in maintenance instructions; no loop.md exists".to_owned()
            }
        };
        format!("Loop instructions ({origin}):\n\n{}", self.text)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("cannot read {}: {source}", path.display())]
pub struct LoopFileError {
    pub path: PathBuf,
    #[source]
    pub source: io::Error,
}

/// The loop.md files a Chat looks for, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoopFiles {
    project: Option<PathBuf>,
    user: Option<PathBuf>,
}

impl LoopFiles {
    pub fn new(folder: Option<&Path>, config_root: Option<&Path>) -> Self {
        Self {
            project: folder.map(|folder| folder.join(PROJECT_LOOP_FILE)),
            user: config_root.map(|root| root.join(LOOP_FILE_NAME)),
        }
    }

    /// Reads the instructions now: the project's loop.md, else the config root's, else the
    /// maintenance instructions. A missing or blank file counts as absent; an unreadable one is
    /// an error, so the loop never falls back to other work than the user configured.
    pub fn instructions(&self) -> Result<LoopInstructions, LoopFileError> {
        let candidates = [
            self.project
                .as_ref()
                .map(|path| (path, LoopInstructionsSource::Project(path.clone()))),
            self.user
                .as_ref()
                .map(|path| (path, LoopInstructionsSource::User(path.clone()))),
        ];
        for (path, source) in candidates.into_iter().flatten() {
            let content = match fs::read_to_string(path) {
                Ok(content) => content,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::IsADirectory
                    ) =>
                {
                    continue;
                }
                Err(source) => {
                    return Err(LoopFileError {
                        path: path.clone(),
                        source,
                    });
                }
            };
            let content = content.trim();
            if content.is_empty() {
                continue;
            }
            let (text, truncated) = truncate_loop_file(content);
            return Ok(LoopInstructions {
                source,
                text,
                truncated,
            });
        }
        Ok(LoopInstructions {
            source: LoopInstructionsSource::Maintenance,
            text: MAINTENANCE_INSTRUCTIONS.trim().to_owned(),
            truncated: false,
        })
    }
}

/// Cuts `content` to at most [`MAX_LOOP_FILE_BYTES`], at the last line break when there is one,
/// and appends a note saying so.
fn truncate_loop_file(content: &str) -> (String, bool) {
    if content.len() <= MAX_LOOP_FILE_BYTES {
        return (content.to_owned(), false);
    }
    let mut end = MAX_LOOP_FILE_BYTES;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    let end = content[..end]
        .rfind('\n')
        .filter(|end| *end > 0)
        .unwrap_or(end);
    (
        format!(
            "{}\n\n> loop.md was truncated to {MAX_LOOP_FILE_BYTES} bytes. Keep it concise.",
            content[..end].trim_end()
        ),
        true,
    )
}

/// The `/loop` Skill body followed by the host's reading of `arguments`: the form, what to run
/// now and the scheduled prompt for CronCreate or ScheduleWakeup.
pub(crate) fn expand(
    body: &str,
    arguments: &str,
    files: &LoopFiles,
) -> Result<String, LoopFileError> {
    let input = arguments.trim();
    let self_paced_prompt = if input.is_empty() {
        LOOP_COMMAND.to_owned()
    } else {
        format!("{LOOP_COMMAND} {input}")
    };
    let invocation = match parse_loop_arguments(input) {
        LoopRequest::FixedInterval { interval, prompt } => format!(
            "Form: fixed interval, every {interval}\nRun now: {prompt}\nScheduled prompt: {prompt}"
        ),
        LoopRequest::SelfPaced { prompt } => {
            format!("Form: self-paced\nRun now: {prompt}\nScheduled prompt: {self_paced_prompt}")
        }
        LoopRequest::DefaultInstructions { interval } => {
            let (form, scheduled_prompt) = match interval {
                Some(interval) => (
                    format!("no prompt, fixed interval, every {interval}"),
                    DEFAULT_INSTRUCTIONS_PROMPT,
                ),
                None => (
                    "no prompt, self-paced".to_owned(),
                    self_paced_prompt.as_str(),
                ),
            };
            format!(
                "Form: {form}\nRun now: the loop instructions below\nScheduled prompt: {scheduled_prompt}\n\n{}",
                files.instructions()?.prompt()
            )
        }
    };
    Ok(format!(
        "{body}\n\n## This /loop\n\nInput: {}\n{invocation}",
        if input.is_empty() { "(none)" } else { input }
    ))
}

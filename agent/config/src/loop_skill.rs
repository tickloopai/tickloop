//! The input of the built-in `/loop` Skill and the loop instructions it runs without a prompt.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::skills::SkillError;
use tickloop_agent_cron::{
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
const LOOP_FILE_NAME: &str = "loop.md";
const PROJECT_LOOP_FILE: &str = ".tickloop/loop.md";
/// Reading this far past the limit is enough to tell a file was cut off.
const READ_PAST_LIMIT_BYTES: u64 = 4;

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
        tickloop_agent_protocol::loop_instructions_prompt(&self.origin(), &self.text)
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
    /// maintenance instructions. A missing or blank file, or a directory, counts as absent; a
    /// symbolic link, another kind of file or an unreadable one is an error, so the loop never
    /// falls back to other work than the user configured.
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
            let Some((text, truncated)) = read_loop_file(path).map_err(|source| LoopFileError {
                path: path.clone(),
                source,
            })?
            else {
                continue;
            };
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

/// The trimmed text of the loop.md at `path` and whether it was cut off, or `None` when the file
/// is absent or blank. At most [`MAX_LOOP_FILE_BYTES`] are kept; the read itself stops just past
/// that, and a cut lands at the last line break, else at a character boundary.
fn read_loop_file(path: &Path) -> io::Result<Option<(String, bool)>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        return Ok(None);
    }
    if file_type.is_symlink() || !file_type.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "loop.md must be a regular file, not a symbolic link or special file",
        ));
    }
    let file = fs::File::open(path)?;
    let opened = file.metadata()?;
    // The path could be swapped for a link between the check and the open.
    if !opened.is_file() || (opened.dev(), opened.ino()) != (metadata.dev(), metadata.ino()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "loop.md changed while it was being opened",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_LOOP_FILE_BYTES as u64 + READ_PAST_LIMIT_BYTES)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > MAX_LOOP_FILE_BYTES;
    let kept = &bytes[..bytes.len().min(MAX_LOOP_FILE_BYTES)];
    let content = match std::str::from_utf8(kept) {
        Ok(content) => content,
        // The cut split a character; keep the complete ones before it.
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&kept[..error.valid_up_to()]).expect("valid up to the cut")
        }
        Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
    };
    let content = content.trim();
    if content.is_empty() {
        return Ok(None);
    }
    if !truncated {
        return Ok(Some((content.to_owned(), false)));
    }
    let end = content
        .rfind('\n')
        .filter(|end| *end > 0)
        .unwrap_or(content.len());
    Ok(Some((
        format!(
            "{}\n\n> loop.md was truncated to {MAX_LOOP_FILE_BYTES} bytes. Keep it concise.",
            content[..end].trim_end()
        ),
        true,
    )))
}

/// The `/loop` Skill body, then any loop instructions as data, then last the host's reading of
/// `arguments`: the form, the interval actually used with its cron expression, what to run now
/// and the scheduled prompt for CronCreate or ScheduleWakeup.
pub(crate) fn expand(body: &str, arguments: &str, files: &LoopFiles) -> Result<String, SkillError> {
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

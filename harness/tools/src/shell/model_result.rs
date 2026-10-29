//! What the model receives from `Bash` and `TaskStop`, following Claude Code's
//! `BashTool.mapToolResultToToolResultBlockParam`, `formatError` (`utils/toolErrors.ts`),
//! `interpretCommandResult` (`BashTool/commandSemantics.ts`) and `TaskStopTool`.

use std::{io::Read, path::Path, time::Duration};
use tickloop_harness_protocol::ShellTaskId;
use tickloop_ids::IdContext;

use serde::Serialize;

use super::open_existing;
use super::{
    CANCELLED_COMMAND_REASON, MAX_COMMAND_OUTPUT_BYTES, OUTPUT_LIMIT_REASON, TIMED_OUT_REASON,
    syntax,
};
use crate::{ToolError, format_file_size, io_error, large_output_message};
use tickloop_harness_protocol::{CommandSnapshot, ToolResult};

/// Claude Code `BASH_MAX_OUTPUT_DEFAULT`: how much of the output file a result carries inline.
const MAX_INLINE_OUTPUT_BYTES: u64 = 30_000;
/// Claude Code `formatError` keeps half of this from each end of a longer error.
const MAX_ERROR_CHARACTERS: usize = 10_000;
const ABORTED_NOTICE: &str = "<error>Command was aborted before completion</error>";
const EMPTY_OUTPUT: &str = "(Bash completed with no output)";
const FAILED_WITHOUT_OUTPUT: &str = "Command failed with no output";
const SIGNAL_EXIT_CODE_BASE: i32 = 128;
const SIGTERM: i32 = 15;
const FALLBACK_EXIT_CODE: i32 = 1;
const BYTES_PER_KILOBYTE: u64 = 1024;
const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 3600;
const TASK_TYPE: &str = "local_bash";
/// Commands whose exit code 1 answers the question instead of reporting a failure.
const EXIT_ONE_IS_AN_ANSWER: [&str; 6] = ["grep", "rg", "find", "diff", "test", "["];

/// Claude Code's result for a command now in the background; one the model asked to background or
/// that outlived its foreground timeout reads as running in the background.
pub(super) fn background_started(
    ids: &IdContext,
    snapshot: &CommandSnapshot,
    is_backgrounded_by_user: bool,
) -> ToolResult {
    let lead = if is_backgrounded_by_user {
        "Command was manually backgrounded by user with ID"
    } else {
        "Command running in background with ID"
    };
    ToolResult {
        content: format!(
            "{lead}: {}. Output is being written to: {}",
            ids.model_text(ShellTaskId::from(snapshot.operation_id)),
            snapshot.output_ref.display()
        ),
        is_error: false,
        output_ref: Some(snapshot.output_ref.clone()),
    }
}

/// The result of a command that has finished; `timeout` is the foreground limit it ran under.
pub(super) fn finished(snapshot: &CommandSnapshot, timeout: Duration) -> ToolResult {
    let path = snapshot.output_ref.as_path();
    let (content, is_error) = match OutputHead::read(path) {
        Err(error) => (error.to_string(), true),
        Ok(output) if snapshot.reason.as_deref() == Some(CANCELLED_COMMAND_REASON) => (
            join_nonempty([output.shown_inline(path).as_str(), ABORTED_NOTICE]),
            true,
        ),
        Ok(output) => {
            let exit_code = exit_code(snapshot);
            let note = snapshot
                .reason
                .as_deref()
                .map(|reason| failure_note(reason, timeout));
            if note.is_some() || is_failed_exit(&snapshot.command, exit_code) {
                (error_message(exit_code, note, &output, path), true)
            } else {
                let shown = output.shown_inline(path);
                if shown.is_empty() {
                    (EMPTY_OUTPUT.to_owned(), false)
                } else {
                    (shown, false)
                }
            }
        }
    };
    ToolResult {
        content,
        is_error,
        output_ref: Some(snapshot.output_ref.clone()),
    }
}

pub(super) fn task_stopped(
    ids: &IdContext,
    snapshot: &CommandSnapshot,
) -> Result<ToolResult, ToolError> {
    #[derive(Serialize)]
    struct TaskStopOutput<'a> {
        message: String,
        task_id: String,
        task_type: &'static str,
        command: &'a str,
    }
    let task_id = ids.model_text(ShellTaskId::from(snapshot.operation_id));
    let output = TaskStopOutput {
        message: format!(
            "Successfully stopped task: {task_id} ({})",
            snapshot.command
        ),
        task_id,
        task_type: TASK_TYPE,
        command: &snapshot.command,
    };
    Ok(ToolResult {
        content: serde_json::to_string(&output)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?,
        is_error: false,
        output_ref: Some(snapshot.output_ref.clone()),
    })
}

/// The beginning of a command's output file, as much as a result carries inline.
struct OutputHead {
    text: String,
    total_bytes: u64,
    is_complete: bool,
}

impl OutputHead {
    fn read(path: &Path) -> Result<Self, ToolError> {
        let file = open_existing(path, false)?;
        let total_bytes = file
            .metadata()
            .map_err(|error| io_error(path, error))?
            .len();
        let mut bytes = Vec::new();
        file.take(MAX_INLINE_OUTPUT_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|error| io_error(path, error))?;
        let is_complete = bytes.len() as u64 >= total_bytes;
        if !is_complete
            && let Err(error) = std::str::from_utf8(&bytes)
            && error.error_len().is_none()
        {
            bytes.truncate(error.valid_up_to());
        }
        Ok(Self {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            total_bytes,
            is_complete,
        })
    }

    /// The output without blank leading lines and trailing whitespace, or Claude Code's large
    /// output message pointing at the whole file when it does not fit inline.
    fn shown_inline(&self, path: &Path) -> String {
        let trimmed = trim_blank_edges(&self.text);
        if self.is_complete {
            trimmed.to_owned()
        } else {
            large_output_message(path, self.total_bytes as usize, trimmed)
        }
    }

    /// Claude Code's notice for output the result does not carry in full.
    fn truncation_notice(&self, path: &Path) -> String {
        format!(
            "Output truncated ({}KB total). Full output saved to: {}",
            (self.total_bytes as f64 / BYTES_PER_KILOBYTE as f64).round(),
            path.display()
        )
    }
}

/// Claude Code's `formatError` parts for a `ShellError`: the exit code, what ended the command, and
/// the raw output. The file path is added whenever the model would not see the whole output.
fn error_message(exit_code: i32, note: Option<String>, output: &OutputHead, path: &Path) -> String {
    let exit_line = format!("Exit code {exit_code}");
    let note = note.unwrap_or_default();
    let mut message = join_nonempty([exit_line.as_str(), &note, &output.text]);
    if !output.is_complete || message.chars().count() > MAX_ERROR_CHARACTERS {
        let notice = output.truncation_notice(path);
        message = join_nonempty([exit_line.as_str(), &note, &notice, &output.text]);
    }
    let message = message.trim();
    if message.is_empty() {
        return FAILED_WITHOUT_OUTPUT.into();
    }
    let length = message.chars().count();
    if length <= MAX_ERROR_CHARACTERS {
        return message.to_owned();
    }
    let half = MAX_ERROR_CHARACTERS / 2;
    let start = message.chars().take(half).collect::<String>();
    let end = message.chars().skip(length - half).collect::<String>();
    format!(
        "{start}\n\n... [{} characters truncated] ...\n\n{end}",
        length - MAX_ERROR_CHARACTERS
    )
}

/// What ended a command early, in the words Claude Code's `ShellCommand` uses where it has them.
fn failure_note(reason: &str, timeout: Duration) -> String {
    match reason {
        TIMED_OUT_REASON => format!("Command timed out after {}", format_duration(timeout)),
        OUTPUT_LIMIT_REASON => format!(
            "Command killed: output file exceeded {}",
            format_file_size(MAX_COMMAND_OUTPUT_BYTES as usize)
        ),
        other => other.to_owned(),
    }
}

/// The exit code a shell would report: a signal `n` becomes `128 + n`, so a timed-out command
/// reads 143 as in Claude Code.
fn exit_code(snapshot: &CommandSnapshot) -> i32 {
    if snapshot.reason.as_deref() == Some(TIMED_OUT_REASON) {
        return SIGNAL_EXIT_CODE_BASE + SIGTERM;
    }
    snapshot
        .exit_code
        .or(snapshot.signal.map(|signal| SIGNAL_EXIT_CODE_BASE + signal))
        .unwrap_or(FALLBACK_EXIT_CODE)
}

/// Claude Code `interpretCommandResult`: the last command of a list decides, and a few commands use
/// exit code 1 for "no match" or "differs".
fn is_failed_exit(command: &str, exit_code: i32) -> bool {
    let last_command = syntax::parse(command)
        .and_then(|commands| commands.last().map(|command| command.text.clone()))
        .unwrap_or_else(|| {
            command
                .rsplit(['|', '&', ';'])
                .next()
                .unwrap_or(command)
                .to_owned()
        });
    let base_command = last_command.split_whitespace().next().unwrap_or_default();
    if EXIT_ONE_IS_AN_ANSWER.contains(&base_command) {
        exit_code >= 2
    } else {
        exit_code != 0
    }
}

/// Claude Code's `stripEmptyLines` and leading-blank-line removal, then `trimEnd`.
fn trim_blank_edges(text: &str) -> &str {
    let text = text.trim_end();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        if !line.ends_with('\n') || !line.trim().is_empty() {
            break;
        }
        start += line.len();
    }
    &text[start..]
}

fn join_nonempty<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Claude Code `formatDuration` for durations under a day.
fn format_duration(duration: Duration) -> String {
    if duration < Duration::from_secs(SECONDS_PER_MINUTE) {
        return format!("{}s", duration.as_secs());
    }
    let total_seconds = duration.as_secs_f64().round() as u64;
    let hours = total_seconds / SECONDS_PER_HOUR;
    let minutes = total_seconds % SECONDS_PER_HOUR / SECONDS_PER_MINUTE;
    let seconds = total_seconds % SECONDS_PER_MINUTE;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else {
        format!("{minutes}m {seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use uuid::Uuid;

    use super::*;
    use tickloop_harness_protocol::CommandState;

    fn snapshot(directory: &Path, command: &str, output: &[u8]) -> CommandSnapshot {
        let directory = directory.canonicalize().unwrap();
        let output_ref = directory.join("task.output");
        std::fs::write(&output_ref, output).unwrap();
        CommandSnapshot {
            updated_at_ms: None,
            operation_id: Uuid::nil(),
            loop_id: Uuid::nil(),
            command: command.into(),
            description: None,
            cwd: directory,
            pid: None,
            state: CommandState::Completed,
            was_background: false,
            backgrounded_on_timeout: false,
            exit_code: Some(0),
            signal: None,
            reason: None,
            output_bytes: output.len() as u64,
            output_ref,
        }
    }

    const TWO_MINUTES: Duration = Duration::from_secs(120);

    #[test]
    fn a_successful_command_returns_its_output_without_blank_edges() {
        let directory = tempfile::tempdir().unwrap();
        let command = snapshot(directory.path(), "ls", b"\n  \nfirst\n  second  \n\n");
        let result = finished(&command, TWO_MINUTES);
        assert_eq!(result.content, "first\n  second");
        assert!(!result.is_error);
        assert_eq!(result.output_ref, Some(command.output_ref));

        let silent = snapshot(directory.path(), "true", b"");
        assert_eq!(finished(&silent, TWO_MINUTES).content, EMPTY_OUTPUT);
    }

    #[test]
    fn a_failing_command_leads_with_its_exit_code() {
        let directory = tempfile::tempdir().unwrap();
        let mut command = snapshot(directory.path(), "make build", b"compiling\nerror: boom\n");
        command.state = CommandState::Failed;
        command.exit_code = Some(2);
        let result = finished(&command, TWO_MINUTES);
        assert_eq!(result.content, "Exit code 2\ncompiling\nerror: boom");
        assert!(result.is_error);

        command.output_bytes = 0;
        std::fs::write(&command.output_ref, b"").unwrap();
        assert_eq!(finished(&command, TWO_MINUTES).content, "Exit code 2");
    }

    #[test]
    fn exit_code_one_answers_search_and_comparison_commands() {
        let directory = tempfile::tempdir().unwrap();
        for (command, is_error) in [
            ("grep missing notes.txt", false),
            ("cat notes.txt | grep -c 'a|b'", false),
            ("cd reports && diff a.txt b.txt", false),
            ("test -f missing.txt", false),
            ("grep missing notes.txt; ls missing", true),
            ("ls missing", true),
        ] {
            let mut failed = snapshot(directory.path(), command, b"");
            failed.state = CommandState::Failed;
            failed.exit_code = Some(1);
            let result = finished(&failed, TWO_MINUTES);
            assert_eq!(result.is_error, is_error, "{command}");
            assert_eq!(
                result.content.starts_with("Exit code 1"),
                is_error,
                "{command}"
            );
        }
        let mut broken = snapshot(directory.path(), "grep -P '(' notes.txt", b"bad regex\n");
        broken.exit_code = Some(2);
        assert_eq!(
            finished(&broken, TWO_MINUTES).content,
            "Exit code 2\nbad regex"
        );
    }

    #[test]
    fn interruptions_timeouts_and_limits_explain_how_the_command_ended() {
        let directory = tempfile::tempdir().unwrap();
        let mut stopped = snapshot(directory.path(), "sleep 30", b"partial\n");
        stopped.reason = Some(CANCELLED_COMMAND_REASON.into());
        stopped.exit_code = None;
        stopped.signal = Some(15);
        let result = finished(&stopped, TWO_MINUTES);
        assert_eq!(result.content, format!("partial\n{ABORTED_NOTICE}"));
        assert!(result.is_error);

        let mut timed_out = snapshot(directory.path(), "sleep 300", b"waiting\n");
        timed_out.reason = Some(TIMED_OUT_REASON.into());
        timed_out.exit_code = None;
        timed_out.signal = Some(15);
        assert_eq!(
            finished(&timed_out, TWO_MINUTES).content,
            "Exit code 143\nCommand timed out after 2m 0s\nwaiting"
        );
        assert_eq!(
            finished(&timed_out, Duration::from_millis(1500)).content,
            "Exit code 143\nCommand timed out after 1s\nwaiting"
        );
        assert_eq!(format_duration(Duration::from_millis(119_500)), "2m 0s");
        assert_eq!(format_duration(Duration::from_millis(600_000)), "10m 0s");
        assert_eq!(
            format_duration(Duration::from_millis(3_725_000)),
            "1h 2m 5s"
        );

        let mut flooded = snapshot(directory.path(), "yes", b"y\n");
        flooded.reason = Some(OUTPUT_LIMIT_REASON.into());
        flooded.exit_code = None;
        flooded.signal = Some(15);
        assert_eq!(
            finished(&flooded, TWO_MINUTES).content,
            "Exit code 143\nCommand killed: output file exceeded 64MB\ny"
        );

        let mut unresolved = snapshot(directory.path(), "sleep 30 &", b"");
        unresolved.state = CommandState::Unknown;
        unresolved.reason = Some("descendants are unresolved".into());
        let result = finished(&unresolved, TWO_MINUTES);
        assert_eq!(result.content, "Exit code 0\ndescendants are unresolved");
        assert!(result.is_error);
    }

    #[test]
    fn output_beyond_the_inline_limit_points_at_the_whole_file() {
        let directory = tempfile::tempdir().unwrap();
        let line = "delivery item\n";
        let output = line.repeat(5000);
        let command = snapshot(directory.path(), "report", output.as_bytes());
        let result = finished(&command, TWO_MINUTES);
        assert!(result.content.starts_with(&format!(
            "<persisted-output>\nOutput too large (68.4KB). Full output saved to: {}\n\nPreview (first 2KB):\ndelivery item\n",
            command.output_ref.display()
        )));
        assert!(result.content.ends_with("\n...\n</persisted-output>"));
        assert!(!result.is_error);

        let mut failed = snapshot(directory.path(), "report", output.as_bytes());
        failed.exit_code = Some(1);
        let result = finished(&failed, TWO_MINUTES);
        assert!(result.content.starts_with(&format!(
            "Exit code 1\nOutput truncated (68KB total). Full output saved to: {}\ndelivery item\n",
            failed.output_ref.display()
        )));
        assert!(result.content.contains("characters truncated] ..."));
        assert!(result.content.chars().count() < MAX_ERROR_CHARACTERS + 100);
    }

    #[test]
    fn a_long_failure_that_fits_inline_is_cut_in_the_middle_with_its_path() {
        let directory = tempfile::tempdir().unwrap();
        let output = format!("{}{}", "a".repeat(8_000), "z".repeat(8_000));
        let mut command = snapshot(directory.path(), "build", output.as_bytes());
        command.exit_code = Some(1);
        let result = finished(&command, TWO_MINUTES);
        let notice = format!(
            "Output truncated (16KB total). Full output saved to: {}",
            command.output_ref.display()
        );
        assert!(
            result
                .content
                .starts_with(&format!("Exit code 1\n{notice}\naaa"))
        );
        assert!(result.content.ends_with(&"z".repeat(5_000)));
    }

    #[test]
    fn background_starts_and_stops_name_the_task_and_its_output_file() {
        let directory = tempfile::tempdir().unwrap();
        let mut command = snapshot(directory.path(), "npm run dev", b"");
        command.operation_id = Uuid::from_u128(7);
        let path = command.output_ref.display();
        let ids = IdContext::new();
        let task = ids.model_text(ShellTaskId::from(command.operation_id));
        assert!(task.starts_with("shell:"));
        assert_eq!(
            background_started(&ids, &command, false).content,
            format!(
                "Command running in background with ID: {task}. Output is being written to: {path}"
            )
        );
        assert_eq!(
            background_started(&ids, &command, true).content,
            format!(
                "Command was manually backgrounded by user with ID: {task}. Output is being written to: {path}"
            )
        );
        assert_eq!(
            task_stopped(&ids, &command).unwrap().content,
            format!(
                r#"{{"message":"Successfully stopped task: {task} (npm run dev)","task_id":"{task}","task_type":"local_bash","command":"npm run dev"}}"#
            )
        );
    }

    #[test]
    fn a_missing_output_file_is_reported() {
        let directory = tempfile::tempdir().unwrap();
        let mut command = snapshot(directory.path(), "true", b"");
        command.output_ref = PathBuf::from("/nonexistent/tickloop/task.output");
        let result = finished(&command, TWO_MINUTES);
        assert!(result.is_error);
        assert!(result.content.contains("/nonexistent/tickloop/task.output"));
    }
}

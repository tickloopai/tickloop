use std::{
    env,
    ffi::{OsStr, OsString},
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(test)]
use std::sync::{Arc, Mutex};

const MAX_PROMPT_BYTES: usize = 8 * 1024 * 1024;
const TEMPORARY_FILE_MODE: u32 = 0o600;
const FALLBACK_EDITORS: [&str; 3] = ["code", "vi", "nano"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExternalEditor {
    pub(super) program: OsString,
    pub(super) arguments: Vec<OsString>,
}

/// How `Ctrl+G` reaches an editor. Tests script the editor so key handling can
/// be checked without handing the terminal to a real process.
#[derive(Clone, Debug)]
pub(super) enum ExternalEditorLauncher {
    Terminal,
    #[cfg(test)]
    Scripted {
        opened_drafts: Arc<Mutex<Vec<String>>>,
        outcome: ExternalEditorOutcome,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ExternalEditorOutcome {
    Updated { content: String },
    Unchanged,
    Cancelled { signal: Option<i32> },
    Unavailable,
    Failed(ExternalEditorFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ExternalEditorFailure {
    InvalidConfiguration {
        variable: &'static str,
        reason: String,
    },
    DraftTooLarge {
        bytes: usize,
        limit: usize,
    },
    EditedPromptTooLarge {
        bytes: usize,
        limit: usize,
    },
    DisallowedControlCharacter {
        byte_offset: usize,
        codepoint: u32,
    },
    TemporaryFile {
        reason: String,
    },
    Launch {
        editor: String,
        reason: String,
    },
    Exit {
        editor: String,
        code: i32,
    },
    Read {
        reason: String,
    },
    InvalidUtf8,
}

impl fmt::Display for ExternalEditorFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration { variable, reason } => {
                write!(formatter, "invalid {variable}: {reason}")
            }
            Self::DraftTooLarge { bytes, limit } => {
                write!(formatter, "draft is {bytes} bytes; limit is {limit}")
            }
            Self::EditedPromptTooLarge { bytes, limit } => {
                write!(
                    formatter,
                    "edited prompt is {bytes} bytes; limit is {limit}"
                )
            }
            Self::DisallowedControlCharacter {
                byte_offset,
                codepoint,
            } => write!(
                formatter,
                "prompt contains disallowed control character U+{codepoint:04X} at byte {byte_offset}"
            ),
            Self::TemporaryFile { reason } => {
                write!(formatter, "cannot prepare external editor file: {reason}")
            }
            Self::Launch { editor, reason } => {
                write!(formatter, "cannot launch {editor}: {reason}")
            }
            Self::Exit { editor, code } => write!(formatter, "{editor} exited with code {code}"),
            Self::Read { reason } => write!(formatter, "cannot read edited prompt: {reason}"),
            Self::InvalidUtf8 => write!(formatter, "edited prompt is not valid UTF-8"),
        }
    }
}

impl std::error::Error for ExternalEditorFailure {}

impl ExternalEditor {
    pub(super) fn from_environment() -> Result<Option<Self>, ExternalEditorFailure> {
        let visual = environment_editor("VISUAL")?;
        let editor = environment_editor("EDITOR")?;
        Self::resolve(
            visual.as_deref(),
            editor.as_deref(),
            env::var_os("PATH").as_deref(),
        )
    }

    fn resolve(
        visual: Option<&str>,
        editor: Option<&str>,
        path: Option<&OsStr>,
    ) -> Result<Option<Self>, ExternalEditorFailure> {
        if let Some(value) = visual.filter(|value| !value.trim().is_empty()) {
            return Self::parse(value, "VISUAL").map(Some);
        }
        if let Some(value) = editor.filter(|value| !value.trim().is_empty()) {
            return Self::parse(value, "EDITOR").map(Some);
        }
        Ok(find_fallback_editor(path))
    }

    fn parse(value: &str, variable: &'static str) -> Result<Self, ExternalEditorFailure> {
        let mut parts = parse_command_line(value)
            .map_err(|reason| ExternalEditorFailure::InvalidConfiguration { variable, reason })?;
        if parts.is_empty() {
            return Err(ExternalEditorFailure::InvalidConfiguration {
                variable,
                reason: "editor command is empty".into(),
            });
        }
        if parts[0].is_empty() {
            return Err(ExternalEditorFailure::InvalidConfiguration {
                variable,
                reason: "editor program is empty".into(),
            });
        }
        let program = OsString::from(parts.remove(0));
        let mut arguments = parts.into_iter().map(OsString::from).collect::<Vec<_>>();
        match value.trim() {
            "code" => arguments.push("-w".into()),
            "subl" => arguments.push("--wait".into()),
            _ => {}
        }
        Ok(Self { program, arguments })
    }

    fn display_name(&self) -> String {
        Path::new(&self.program)
            .file_name()
            .unwrap_or(&self.program)
            .to_string_lossy()
            .into_owned()
    }
}

pub(super) fn edit_prompt(draft: &str, working_directory: &Path) -> ExternalEditorOutcome {
    edit_prompt_with_cancellation(draft, working_directory, None)
}

pub(super) fn edit_prompt_with_cancellation(
    draft: &str,
    working_directory: &Path,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
) -> ExternalEditorOutcome {
    let editor = match ExternalEditor::from_environment() {
        Ok(Some(editor)) => editor,
        Ok(None) => return ExternalEditorOutcome::Unavailable,
        Err(error) => return ExternalEditorOutcome::Failed(error),
    };
    match cancellation {
        Some(cancellation) => edit_prompt_with_limit_and_cancellation(
            &editor,
            draft,
            working_directory,
            MAX_PROMPT_BYTES,
            Some(cancellation),
        ),
        None => edit_prompt_with(&editor, draft, working_directory),
    }
}

/// Opens `path` itself in the external editor, as `/plan open` does for the plan file.
pub(super) fn edit_file(path: &Path, working_directory: &Path) -> ExternalEditorOutcome {
    match ExternalEditor::from_environment() {
        Ok(Some(editor)) => edit_file_with(&editor, path, working_directory),
        Ok(None) => ExternalEditorOutcome::Unavailable,
        Err(error) => ExternalEditorOutcome::Failed(error),
    }
}

fn edit_file_with(
    editor: &ExternalEditor,
    path: &Path,
    working_directory: &Path,
) -> ExternalEditorOutcome {
    let read = |path: &Path| {
        read_bounded(path, MAX_PROMPT_BYTES)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    };
    let before = match read(path) {
        Ok(text) => text,
        Err(error) => return ExternalEditorOutcome::Failed(error),
    };
    let status = match Command::new(&editor.program)
        .args(&editor.arguments)
        .arg(path)
        .current_dir(working_directory)
        .status()
    {
        Ok(status) => status,
        Err(error) => {
            return ExternalEditorOutcome::Failed(ExternalEditorFailure::Launch {
                editor: editor.display_name(),
                reason: error.to_string(),
            });
        }
    };
    if let Some(signal) = status.signal() {
        return ExternalEditorOutcome::Cancelled {
            signal: Some(signal),
        };
    }
    if !status.success() {
        return ExternalEditorOutcome::Failed(ExternalEditorFailure::Exit {
            editor: editor.display_name(),
            code: status.code().unwrap_or(-1),
        });
    }
    match read(path) {
        Ok(after) if after == before => ExternalEditorOutcome::Unchanged,
        Ok(content) => ExternalEditorOutcome::Updated { content },
        Err(error) => ExternalEditorOutcome::Failed(error),
    }
}

pub(super) fn edit_prompt_with(
    editor: &ExternalEditor,
    draft: &str,
    working_directory: &Path,
) -> ExternalEditorOutcome {
    edit_prompt_with_limit(editor, draft, working_directory, MAX_PROMPT_BYTES)
}

fn edit_prompt_with_limit(
    editor: &ExternalEditor,
    draft: &str,
    working_directory: &Path,
    byte_limit: usize,
) -> ExternalEditorOutcome {
    edit_prompt_with_limit_and_cancellation(editor, draft, working_directory, byte_limit, None)
}

fn edit_prompt_with_limit_and_cancellation(
    editor: &ExternalEditor,
    draft: &str,
    working_directory: &Path,
    byte_limit: usize,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
) -> ExternalEditorOutcome {
    if draft.len() > byte_limit {
        return ExternalEditorOutcome::Failed(ExternalEditorFailure::DraftTooLarge {
            bytes: draft.len(),
            limit: byte_limit,
        });
    }
    if let Some(error) = disallowed_control_character(draft) {
        return ExternalEditorOutcome::Failed(error);
    }
    let temporary = match TemporaryPrompt::create(draft) {
        Ok(temporary) => temporary,
        Err(error) => return ExternalEditorOutcome::Failed(error),
    };
    let mut command = Command::new(&editor.program);
    command
        .args(&editor.arguments)
        .arg(temporary.path())
        .current_dir(working_directory);
    let waited = if let Some(cancellation) = cancellation {
        command.spawn().and_then(|mut child| {
            loop {
                if cancellation.is_cancelled() {
                    let _ = child.kill();
                    return child.wait();
                }
                if let Some(status) = child.try_wait()? {
                    return Ok(status);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        })
    } else {
        command.status()
    };
    let status = match waited {
        Ok(status) => status,
        Err(error) => {
            return ExternalEditorOutcome::Failed(ExternalEditorFailure::Launch {
                editor: editor.display_name(),
                reason: error.to_string(),
            });
        }
    };
    if let Some(signal) = status.signal() {
        return ExternalEditorOutcome::Cancelled {
            signal: Some(signal),
        };
    }
    if !status.success() {
        return ExternalEditorOutcome::Failed(ExternalEditorFailure::Exit {
            editor: editor.display_name(),
            code: status.code().unwrap_or(-1),
        });
    }
    let bytes = match read_bounded(temporary.path(), byte_limit) {
        Ok(bytes) => bytes,
        Err(error) => return ExternalEditorOutcome::Failed(error),
    };
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(_) => {
            return ExternalEditorOutcome::Failed(ExternalEditorFailure::InvalidUtf8);
        }
    };
    if let Some(error) = disallowed_control_character(&content) {
        return ExternalEditorOutcome::Failed(error);
    }
    let content = trim_one_trailing_newline(content);
    if content == draft {
        ExternalEditorOutcome::Unchanged
    } else {
        ExternalEditorOutcome::Updated { content }
    }
}

struct TemporaryPrompt {
    path: PathBuf,
    _directory: tempfile::TempDir,
}

impl TemporaryPrompt {
    fn create(draft: &str) -> Result<Self, ExternalEditorFailure> {
        let directory = tempfile::Builder::new()
            .prefix("tickloop-editor-")
            .tempdir()
            .map_err(|error| ExternalEditorFailure::TemporaryFile {
                reason: error.to_string(),
            })?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).map_err(
            |error| ExternalEditorFailure::TemporaryFile {
                reason: error.to_string(),
            },
        )?;
        let path = directory.path().join("input.md");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(TEMPORARY_FILE_MODE)
            .open(&path)
            .map_err(|error| ExternalEditorFailure::TemporaryFile {
                reason: error.to_string(),
            })?;
        if let Err(error) = file
            .set_permissions(fs::Permissions::from_mode(TEMPORARY_FILE_MODE))
            .and_then(|()| file.write_all(draft.as_bytes()))
            .and_then(|()| file.sync_all())
        {
            let _ = fs::remove_file(&path);
            return Err(ExternalEditorFailure::TemporaryFile {
                reason: error.to_string(),
            });
        }
        Ok(Self {
            path,
            _directory: directory,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryPrompt {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn environment_editor(variable: &'static str) -> Result<Option<String>, ExternalEditorFailure> {
    let Some(value) = env::var_os(variable) else {
        return Ok(None);
    };
    let value = value
        .into_string()
        .map_err(|_| ExternalEditorFailure::InvalidConfiguration {
            variable,
            reason: "value is not valid UTF-8".into(),
        })?;
    let value = value.trim();
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

fn parse_command_line(command: &str) -> Result<Vec<String>, String> {
    #[derive(Clone, Copy)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut arguments = Vec::new();
    let mut argument = String::new();
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut started = false;
    for character in command.chars() {
        if escaped {
            argument.push(character);
            escaped = false;
            started = true;
            continue;
        }
        match quote {
            Quote::Single => {
                if character == '\'' {
                    quote = Quote::None;
                } else {
                    argument.push(character);
                }
                started = true;
            }
            Quote::Double => match character {
                '"' => {
                    quote = Quote::None;
                    started = true;
                }
                '\\' => escaped = true,
                _ => {
                    argument.push(character);
                    started = true;
                }
            },
            Quote::None => match character {
                '\'' => {
                    quote = Quote::Single;
                    started = true;
                }
                '"' => {
                    quote = Quote::Double;
                    started = true;
                }
                '\\' => escaped = true,
                character if character.is_whitespace() => {
                    if started {
                        arguments.push(std::mem::take(&mut argument));
                        started = false;
                    }
                }
                _ => {
                    argument.push(character);
                    started = true;
                }
            },
        }
    }
    if escaped {
        return Err("editor command ends with an escape".into());
    }
    match quote {
        Quote::Single => return Err("editor command has an unclosed single quote".into()),
        Quote::Double => return Err("editor command has an unclosed double quote".into()),
        Quote::None => {}
    }
    if started {
        arguments.push(argument);
    }
    Ok(arguments)
}

fn find_fallback_editor(path: Option<&OsStr>) -> Option<ExternalEditor> {
    FALLBACK_EDITORS.iter().find_map(|editor| {
        find_executable(editor, path).map(|program| ExternalEditor {
            program,
            arguments: if *editor == "code" {
                vec!["-w".into()]
            } else {
                Vec::new()
            },
        })
    })
}

fn find_executable(program: &str, path: Option<&OsStr>) -> Option<OsString> {
    use std::os::unix::fs::PermissionsExt;

    env::split_paths(path?).find_map(|directory| {
        let candidate = directory.join(program);
        let metadata = candidate.metadata().ok()?;
        (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .then(|| candidate.into_os_string())
    })
}

fn read_bounded(path: &Path, byte_limit: usize) -> Result<Vec<u8>, ExternalEditorFailure> {
    let file = File::open(path).map_err(|error| ExternalEditorFailure::Read {
        reason: error.to_string(),
    })?;
    let mut bytes = Vec::new();
    file.take(byte_limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| ExternalEditorFailure::Read {
            reason: error.to_string(),
        })?;
    if bytes.len() > byte_limit {
        return Err(ExternalEditorFailure::EditedPromptTooLarge {
            bytes: bytes.len(),
            limit: byte_limit,
        });
    }
    Ok(bytes)
}

fn disallowed_control_character(content: &str) -> Option<ExternalEditorFailure> {
    content.char_indices().find_map(|(byte_offset, character)| {
        (character.is_control() && !matches!(character, '\n' | '\r' | '\t')).then_some(
            ExternalEditorFailure::DisallowedControlCharacter {
                byte_offset,
                codepoint: character as u32,
            },
        )
    })
}

fn trim_one_trailing_newline(mut content: String) -> String {
    if content.ends_with('\n') && !content.ends_with("\n\n") {
        content.pop();
    }
    content
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use tempfile::TempDir;

    use super::*;

    fn fixture(directory: &TempDir, body: &str) -> PathBuf {
        let path = directory
            .path()
            .join(format!("editor-{}.sh", uuid::Uuid::new_v4()));
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    fn command(program: PathBuf, arguments: Vec<OsString>) -> ExternalEditor {
        ExternalEditor {
            program: program.into_os_string(),
            arguments,
        }
    }

    fn recorded_temporary_path(record: &Path) -> PathBuf {
        PathBuf::from(fs::read_to_string(record).unwrap())
    }

    #[test]
    fn command_parser_preserves_quoted_arguments_without_invoking_a_shell() {
        let editor = ExternalEditor::parse(
            r#""/Applications/My Editor/bin/edit" --wait "" 'two words' escaped\ value"#,
            "VISUAL",
        )
        .unwrap();
        assert_eq!(editor.program, "/Applications/My Editor/bin/edit");
        assert_eq!(
            editor.arguments,
            ["--wait", "", "two words", "escaped value"]
                .map(OsString::from)
                .to_vec()
        );
        assert!(matches!(
            ExternalEditor::parse("vim 'unfinished", "EDITOR"),
            Err(ExternalEditorFailure::InvalidConfiguration {
                variable: "EDITOR",
                ..
            })
        ));
        assert!(matches!(
            ExternalEditor::parse("''", "VISUAL"),
            Err(ExternalEditorFailure::InvalidConfiguration {
                variable: "VISUAL",
                ..
            })
        ));
        assert_eq!(
            ExternalEditor::parse("code", "EDITOR").unwrap().arguments,
            vec![OsString::from("-w")]
        );
    }

    #[test]
    fn trailing_newline_rule_matches_prompt_editor_behavior() {
        assert_eq!(trim_one_trailing_newline("one\n".into()), "one");
        assert_eq!(trim_one_trailing_newline("one\n\n".into()), "one\n\n");
        assert_eq!(trim_one_trailing_newline("one\r\n".into()), "one\r");
    }

    #[test]
    fn visual_precedes_editor_and_blank_values_fall_through() {
        let visual = ExternalEditor::resolve(Some("nvim -f"), Some("nano"), None)
            .unwrap()
            .unwrap();
        assert_eq!(visual.program, "nvim");
        assert_eq!(visual.arguments, vec![OsString::from("-f")]);

        let editor = ExternalEditor::resolve(Some("  "), Some("nano -w"), None)
            .unwrap()
            .unwrap();
        assert_eq!(editor.program, "nano");
        assert_eq!(editor.arguments, vec![OsString::from("-w")]);
        assert_eq!(ExternalEditor::resolve(None, None, None).unwrap(), None);
    }

    #[test]
    fn secure_temporary_prompt_has_owner_only_permissions_and_is_removed_on_drop() {
        let temporary = TemporaryPrompt::create("secret draft").unwrap();
        let path = temporary.path().to_owned();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o077, 0);
        assert_ne!(mode & TEMPORARY_FILE_MODE, 0);
        assert_eq!(fs::read_to_string(&path).unwrap(), "secret draft");
        drop(temporary);
        assert!(!path.exists());
    }

    #[test]
    fn successful_editor_updates_utf8_prompt_trims_one_newline_and_cleans_up() {
        let directory = TempDir::new().unwrap();
        let record = directory.path().join("temporary-path");
        let editor = fixture(
            &directory,
            "record=$1\nfile=$2\nprintf '%s' \"$file\" > \"$record\"\nprintf 'edited 中文\\n' > \"$file\"",
        );
        let outcome = edit_prompt_with(
            &command(editor, vec![record.clone().into_os_string()]),
            "original",
            directory.path(),
        );
        assert_eq!(
            outcome,
            ExternalEditorOutcome::Updated {
                content: "edited 中文".into()
            }
        );
        assert!(!recorded_temporary_path(&record).exists());
    }

    #[test]
    fn a_file_is_edited_in_place_and_reports_whether_it_changed() {
        let directory = TempDir::new().unwrap();
        let file = directory.path().join("plan.md");
        fs::write(&file, "1. Draft.\n").unwrap();
        let appending = fixture(&directory, "printf '2. Test.\\n' >> \"$1\"");
        assert_eq!(
            edit_file_with(&command(appending, Vec::new()), &file, directory.path()),
            ExternalEditorOutcome::Updated {
                content: "1. Draft.\n2. Test.\n".into()
            }
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), "1. Draft.\n2. Test.\n");
        let viewing = fixture(&directory, ":");
        assert_eq!(
            edit_file_with(&command(viewing, Vec::new()), &file, directory.path()),
            ExternalEditorOutcome::Unchanged
        );
        let failing = fixture(&directory, "exit 3");
        assert!(matches!(
            edit_file_with(&command(failing, Vec::new()), &file, directory.path()),
            ExternalEditorOutcome::Failed(ExternalEditorFailure::Exit { code: 3, .. })
        ));
    }

    #[test]
    fn editor_that_leaves_the_draft_unchanged_returns_unchanged_and_cleans_up() {
        let directory = TempDir::new().unwrap();
        let record = directory.path().join("temporary-path");
        let editor = fixture(
            &directory,
            "record=$1\nfile=$2\nprintf '%s' \"$file\" > \"$record\"\n:",
        );
        let outcome = edit_prompt_with(
            &command(editor, vec![record.clone().into_os_string()]),
            "same draft",
            directory.path(),
        );
        assert_eq!(outcome, ExternalEditorOutcome::Unchanged);
        assert!(!recorded_temporary_path(&record).exists());
    }

    #[test]
    fn nonzero_exit_does_not_apply_file_changes_and_cleans_up() {
        let directory = TempDir::new().unwrap();
        let record = directory.path().join("temporary-path");
        let editor = fixture(
            &directory,
            "record=$1\nfile=$2\nprintf '%s' \"$file\" > \"$record\"\nprintf 'must not apply' > \"$file\"\nexit 7",
        );
        let outcome = edit_prompt_with(
            &command(editor, vec![record.clone().into_os_string()]),
            "original",
            directory.path(),
        );
        assert!(matches!(
            outcome,
            ExternalEditorOutcome::Failed(ExternalEditorFailure::Exit { code: 7, .. })
        ));
        assert!(!recorded_temporary_path(&record).exists());
    }

    #[test]
    fn signalled_editor_is_cancelled_and_cleans_up() {
        let directory = TempDir::new().unwrap();
        let record = directory.path().join("temporary-path");
        let editor = fixture(
            &directory,
            "record=$1\nfile=$2\nprintf '%s' \"$file\" > \"$record\"\nkill -TERM $$",
        );
        let outcome = edit_prompt_with(
            &command(editor, vec![record.clone().into_os_string()]),
            "original",
            directory.path(),
        );
        assert_eq!(
            outcome,
            ExternalEditorOutcome::Cancelled { signal: Some(15) }
        );
        assert!(!recorded_temporary_path(&record).exists());
    }

    #[test]
    fn invalid_utf8_and_control_characters_never_replace_the_draft() {
        let directory = TempDir::new().unwrap();
        let invalid = fixture(&directory, "file=$1\nprintf '\\377' > \"$file\"");
        assert_eq!(
            edit_prompt_with(&command(invalid, Vec::new()), "original", directory.path()),
            ExternalEditorOutcome::Failed(ExternalEditorFailure::InvalidUtf8)
        );

        let control = fixture(&directory, "file=$1\nprintf 'bad\\033text' > \"$file\"");
        assert!(matches!(
            edit_prompt_with(&command(control, Vec::new()), "original", directory.path()),
            ExternalEditorOutcome::Failed(ExternalEditorFailure::DisallowedControlCharacter {
                codepoint: 27,
                ..
            })
        ));
        assert!(matches!(
            edit_prompt_with(
                &command(PathBuf::from("unused"), Vec::new()),
                "bad\0draft",
                directory.path()
            ),
            ExternalEditorOutcome::Failed(ExternalEditorFailure::DisallowedControlCharacter {
                codepoint: 0,
                ..
            })
        ));
    }

    #[test]
    fn prompt_size_limit_applies_before_launch_and_after_editing() {
        let directory = TempDir::new().unwrap();
        let unused = command(PathBuf::from("unused"), Vec::new());
        assert_eq!(
            edit_prompt_with_limit(&unused, "12345", directory.path(), 4),
            ExternalEditorOutcome::Failed(ExternalEditorFailure::DraftTooLarge {
                bytes: 5,
                limit: 4,
            })
        );

        let editor = fixture(&directory, "file=$1\nprintf '12345' > \"$file\"");
        assert_eq!(
            edit_prompt_with_limit(&command(editor, Vec::new()), "1234", directory.path(), 4,),
            ExternalEditorOutcome::Failed(ExternalEditorFailure::EditedPromptTooLarge {
                bytes: 5,
                limit: 4,
            })
        );
    }

    #[test]
    fn missing_editor_is_a_structured_launch_failure() {
        let directory = TempDir::new().unwrap();
        let editor = command(directory.path().join("does-not-exist"), Vec::new());
        assert!(matches!(
            edit_prompt_with(&editor, "draft", directory.path()),
            ExternalEditorOutcome::Failed(ExternalEditorFailure::Launch { .. })
        ));
    }

    #[test]
    fn fallback_search_respects_path_order_and_executable_permissions() {
        let directory = TempDir::new().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(first.join("code"), "not executable").unwrap();
        let vi = second.join("vi");
        fs::write(&vi, "#!/bin/sh\n").unwrap();
        let mut permissions = fs::metadata(&vi).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&vi, permissions).unwrap();
        let path = env::join_paths([first, second]).unwrap();
        let editor = find_fallback_editor(Some(&path)).unwrap();
        assert_eq!(editor.program, vi);
        assert!(editor.arguments.is_empty());
    }
}

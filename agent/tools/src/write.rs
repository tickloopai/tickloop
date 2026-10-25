//! Claude Code's `Write` and `Edit` tools (`FileWriteTool.ts`, `FileEditTool.ts`,
//! `FileEditTool/utils.ts`): read-before-write validation against the Chat's read state, a file
//! review prepared before approval, and a publication that refuses files changed after approval.

use std::{
    ffi::CString,
    fs::{File, Metadata},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::Path,
};

use serde::{Deserialize, Deserializer, de::Error as _};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    FileReadRecordExt,
    file_change::{FileChangeReview, build_file_change_review},
};

use super::{
    FileReadState, Operation, READ_TOOL_NAME, ToolDefinition, ToolError, io_error, open_existing,
    read::recorded_text,
};
use tickloop_agent_base::private_fs::{
    OpenTarget, PRIVATE_FILE_MODE, open_without_following_links,
};
use tickloop_agent_protocol::{FileChangeAction, FileIdentity, FileReadRecord, ToolResult};

pub const WRITE_TOOL_NAME: &str = "Write";
pub const EDIT_TOOL_NAME: &str = "Edit";

const MAX_WRITE_BYTES: usize = 8 * 1024 * 1024;
/// Permission bits kept when overwriting an existing file; new files get private file permissions.
const PRESERVED_PERMISSION_BITS: u32 = 0o777;
/// Parent directories created for a new file get the conventional mode, narrowed by the umask.
const NEW_DIRECTORY_MODE: u32 = 0o777;
/// Claude Code decides a file's line endings from its first 4096 characters.
const LINE_ENDING_SAMPLE_CHARS: usize = 4096;

const FILE_NOT_READ: &str = "File has not been read yet. Read it first before writing to it.";
const FILE_MODIFIED_SINCE_READ: &str = "File has been modified since read, either by the user or by a linter. Read it again before attempting to write it.";
const IDENTICAL_STRINGS: &str =
    "No changes to make: old_string and new_string are exactly the same.";
const FILE_ALREADY_EXISTS: &str = "Cannot create new file - file already exists.";
pub(super) const FILE_NOT_FOUND_CWD_NOTE: &str = "Note: your current working directory is";
const EDIT_CHANGES_NOTHING: &str = "Original and edited file match exactly. Failed to apply edit.";

const LEFT_SINGLE_CURLY_QUOTE: char = '\u{2018}';
const RIGHT_SINGLE_CURLY_QUOTE: char = '\u{2019}';
const LEFT_DOUBLE_CURLY_QUOTE: char = '\u{201c}';
const RIGHT_DOUBLE_CURLY_QUOTE: char = '\u{201d}';

#[derive(Clone, Debug)]
pub(super) struct PreparedWrite {
    tool: FileChangeTool,
    change: Option<Change>,
    content: Option<String>,
    expected: Option<FileIdentity>,
    review: Option<Box<FileChangeReview>>,
}

/// The tool that asked for the change, which decides Claude Code's success text.
#[derive(Clone, Copy, Debug)]
enum FileChangeTool {
    Write,
    Edit { replace_all: bool },
}

#[derive(Clone, Debug)]
enum Change {
    Write(String),
    Edit(EditRequest),
}

#[derive(Clone, Debug)]
struct EditRequest {
    old_string: String,
    new_string: String,
    replace_all: bool,
}

pub(super) fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: WRITE_TOOL_NAME.into(),
            description: format!(
                "Writes a file to the local filesystem.

Usage:
- This tool will overwrite the existing file if there is one at the provided path.
- If this is an existing file, you MUST use the {READ_TOOL_NAME} tool first to read the file's contents. This tool will fail if you did not read the file first.
- Prefer the {EDIT_TOOL_NAME} tool for modifying existing files \u{2014} it only sends the diff. Only use this tool to create new files or for complete rewrites.
- NEVER create documentation files (*.md) or README files unless explicitly requested by the User.
- Only use emojis if the user explicitly requests it. Avoid writing emojis to files unless asked."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "The absolute path to the file to write (must be absolute, not relative)"
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write to the file"
                    }
                },
                "required": ["file_path", "content"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: EDIT_TOOL_NAME.into(),
            description: format!(
                "Performs exact string replacements in files.

Usage:
- You must use your `{READ_TOOL_NAME}` tool at least once in the conversation before editing. This tool will error if you attempt an edit without reading the file. 
- When editing text from {READ_TOOL_NAME} tool output, ensure you preserve the exact indentation (tabs/spaces) as it appears AFTER the line number prefix. The line number prefix format is: spaces + line number + arrow. Everything after that is the actual file content to match. Never include any part of the line number prefix in the old_string or new_string.
- ALWAYS prefer editing existing files in the codebase. NEVER write new files unless explicitly required.
- Only use emojis if the user explicitly requests it. Avoid adding emojis to files unless asked.
- The edit will FAIL if `old_string` is not unique in the file. Either provide a larger string with more surrounding context to make it unique or use `replace_all` to change every instance of `old_string`.
- Use `replace_all` for replacing and renaming strings across the file. This parameter is useful if you want to rename a variable for instance."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "The absolute path to the file to modify"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "The text to replace"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "The text to replace it with (must be different from old_string)"
                    },
                    "replace_all": {
                        "type": "boolean",
                        "default": false,
                        "description": "Replace all occurrences of old_string (default false)"
                    }
                },
                "required": ["file_path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        },
    ]
}

pub(super) fn prepare_input(
    name: &str,
    input: &Value,
) -> Result<(String, Operation, Value), ToolError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct WriteInput {
        file_path: String,
        content: String,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EditInput {
        file_path: String,
        old_string: String,
        new_string: String,
        #[serde(default, deserialize_with = "semantic_boolean")]
        replace_all: bool,
    }
    let mut normalized_input = input.clone();
    let (file_path, tool, change) = if name == EDIT_TOOL_NAME {
        let parsed: EditInput = serde_json::from_value(input.clone())
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        if parsed.old_string == parsed.new_string {
            return Err(ToolError::Rejected(IDENTICAL_STRINGS.into()));
        }
        if parsed.old_string.len() + parsed.new_string.len() > MAX_WRITE_BYTES {
            return Err(ToolError::InvalidInput("edit content exceeds 8 MiB".into()));
        }
        if let Some(replace_all) = normalized_input.get_mut("replace_all") {
            *replace_all = json!(parsed.replace_all);
        }
        (
            parsed.file_path,
            FileChangeTool::Edit {
                replace_all: parsed.replace_all,
            },
            Change::Edit(EditRequest {
                old_string: parsed.old_string,
                new_string: parsed.new_string,
                replace_all: parsed.replace_all,
            }),
        )
    } else {
        let parsed: WriteInput = serde_json::from_value(input.clone())
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        if parsed.content.len() > MAX_WRITE_BYTES {
            return Err(ToolError::InvalidInput(
                "write content exceeds 8 MiB".into(),
            ));
        }
        (
            parsed.file_path,
            FileChangeTool::Write,
            Change::Write(parsed.content),
        )
    };
    Ok((
        file_path,
        Operation::Write(Box::new(PreparedWrite {
            tool,
            change: Some(change),
            content: None,
            expected: None,
            review: None,
        })),
        normalized_input,
    ))
}

/// Claude Code `semanticBoolean`: models occasionally quote booleans, so `"true"` and `"false"`
/// count too.
pub(super) fn semantic_boolean<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<bool, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SemanticBoolean {
        Boolean(bool),
        Text(String),
    }
    match SemanticBoolean::deserialize(deserializer)? {
        SemanticBoolean::Boolean(value) => Ok(value),
        SemanticBoolean::Text(text) => match text.as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(D::Error::custom(format!(
                "expected a boolean, found {text:?}"
            ))),
        },
    }
}

/// Binds the requested change to the file as it is now: validates it the way Claude Code's
/// `validateInput` does, then computes the exact content to publish and its review.
pub(super) fn bind(
    path: &Path,
    mut operation: PreparedWrite,
    reads: &FileReadState,
    workdir: &Path,
) -> Result<PreparedWrite, ToolError> {
    let change = operation
        .change
        .take()
        .expect("unbound write operation must contain a requested change");
    let current = read_for_review(path)?;
    let (action, content) = match change {
        Change::Write(content) => {
            if let Some((_, metadata)) = &current {
                verify_read(path, reads, metadata, None)?;
            }
            let action = if current.is_some() {
                FileChangeAction::Overwrite
            } else {
                FileChangeAction::Create
            };
            (action, content)
        }
        Change::Edit(request) => edit(path, current.as_ref(), reads, workdir, &request)?,
    };
    if content.len() > MAX_WRITE_BYTES {
        return Err(ToolError::InvalidInput(
            "result exceeds 8 MiB writing limit".into(),
        ));
    }
    operation.expected = current
        .as_ref()
        .map(|(_, metadata)| FileIdentity::from_metadata(metadata));
    let before = current.as_ref().map_or("", |(content, _)| content.as_str());
    operation.review = Some(Box::new(build_file_change_review(
        action,
        path.to_path_buf(),
        before,
        operation.expected.is_some(),
        operation.expected.clone(),
        &content,
        true,
    )));
    operation.content = Some(content);
    Ok(operation)
}

/// Claude Code's read-before-write check: an existing file must have been read in this Chat and
/// must not have changed since. With `current_text`, a full read whose content still matches
/// passes a newer modification time, as `Edit` allows.
fn verify_read(
    path: &Path,
    reads: &FileReadState,
    metadata: &Metadata,
    current_text: Option<&str>,
) -> Result<(), ToolError> {
    let record = reads
        .get(path)
        .ok_or_else(|| ToolError::Rejected(FILE_NOT_READ.into()))?;
    let modified = metadata.modified().map_err(|error| io_error(path, error))?;
    let is_unchanged_full_read = || {
        record.is_full_read
            && current_text.is_some_and(|text| recorded_text(text) == record.content)
    };
    if modified > record.modified && !is_unchanged_full_read() {
        return Err(ToolError::Rejected(FILE_MODIFIED_SINCE_READ.into()));
    }
    Ok(())
}

/// Validates and applies one `Edit` like Claude Code's `FileEditTool`: the replacement runs on the
/// file with `\r\n` normalized to `\n`, and a file that mostly used `\r\n` keeps it.
fn edit(
    path: &Path,
    current: Option<&(String, Metadata)>,
    reads: &FileReadState,
    workdir: &Path,
    request: &EditRequest,
) -> Result<(FileChangeAction, String), ToolError> {
    let EditRequest {
        old_string,
        new_string,
        replace_all,
    } = request;
    let Some((raw, metadata)) = current else {
        if old_string.is_empty() {
            return Ok((FileChangeAction::Create, new_string.clone()));
        }
        return Err(ToolError::Rejected(format!(
            "File does not exist. {FILE_NOT_FOUND_CWD_NOTE} {}.",
            workdir.display()
        )));
    };
    let original = raw.replace("\r\n", "\n");
    if old_string.is_empty() && !original.trim().is_empty() {
        return Err(ToolError::Rejected(FILE_ALREADY_EXISTS.into()));
    }
    verify_read(path, reads, metadata, Some(raw))?;
    let updated = if old_string.is_empty() {
        new_string.clone()
    } else {
        let actual_old_string = find_actual_string(&original, old_string).ok_or_else(|| {
            ToolError::Rejected(format!(
                "String to replace not found in file.\nString: {old_string}"
            ))
        })?;
        let matches = original.matches(actual_old_string.as_str()).count();
        if matches > 1 && !replace_all {
            return Err(ToolError::Rejected(format!(
                "Found {matches} matches of the string to replace, but replace_all is false. To replace all occurrences, set replace_all to true. To replace only one occurrence, please provide more context to uniquely identify the instance.\nString: {old_string}"
            )));
        }
        let actual_new_string = preserve_quote_style(old_string, &actual_old_string, new_string);
        apply_edit(
            &original,
            &actual_old_string,
            &actual_new_string,
            *replace_all,
        )
    };
    if updated == original {
        return Err(ToolError::Rejected(EDIT_CHANGES_NOTHING.into()));
    }
    let content = if uses_crlf_line_endings(raw) {
        updated.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        updated
    };
    let action = if *replace_all {
        FileChangeAction::ReplaceAll
    } else {
        FileChangeAction::Edit
    };
    Ok((action, content))
}

/// Claude Code `detectLineEndingsForString` over the file's leading sample.
fn uses_crlf_line_endings(text: &str) -> bool {
    let (mut crlf, mut lf) = (0usize, 0usize);
    let mut previous = None;
    for character in text.chars().take(LINE_ENDING_SAMPLE_CHARS) {
        if character == '\n' {
            if previous == Some('\r') {
                crlf += 1;
            } else {
                lf += 1;
            }
        }
        previous = Some(character);
    }
    crlf > lf
}

fn normalize_quote(character: char) -> char {
    match character {
        LEFT_SINGLE_CURLY_QUOTE | RIGHT_SINGLE_CURLY_QUOTE => '\'',
        LEFT_DOUBLE_CURLY_QUOTE | RIGHT_DOUBLE_CURLY_QUOTE => '"',
        other => other,
    }
}

/// Claude Code `findActualString`: the exact text, or else the file's own text that matches once
/// curly quotes on both sides count as straight quotes.
fn find_actual_string(file: &str, search: &str) -> Option<String> {
    if file.contains(search) {
        return Some(search.to_owned());
    }
    let normalized_search = search.chars().map(normalize_quote).collect::<String>();
    let normalized_file = file.chars().map(normalize_quote).collect::<String>();
    let byte_start = normalized_file.find(&normalized_search)?;
    let char_start = normalized_file[..byte_start].chars().count();
    Some(
        file.chars()
            .skip(char_start)
            .take(normalized_search.chars().count())
            .collect(),
    )
}

/// Claude Code `preserveQuoteStyle`: when `old_string` only matched through quote normalization,
/// the straight quotes in `new_string` take the file's curly style.
fn preserve_quote_style(old_string: &str, actual_old_string: &str, new_string: &str) -> String {
    if old_string == actual_old_string {
        return new_string.to_owned();
    }
    let has_double_quotes =
        actual_old_string.contains([LEFT_DOUBLE_CURLY_QUOTE, RIGHT_DOUBLE_CURLY_QUOTE]);
    let has_single_quotes =
        actual_old_string.contains([LEFT_SINGLE_CURLY_QUOTE, RIGHT_SINGLE_CURLY_QUOTE]);
    let characters = new_string.chars().collect::<Vec<_>>();
    characters
        .iter()
        .enumerate()
        .map(|(index, &character)| match character {
            '"' if has_double_quotes => {
                if is_opening_context(&characters, index) {
                    LEFT_DOUBLE_CURLY_QUOTE
                } else {
                    RIGHT_DOUBLE_CURLY_QUOTE
                }
            }
            '\'' if has_single_quotes => {
                let is_letter = |position: Option<usize>| {
                    position
                        .and_then(|position| characters.get(position))
                        .is_some_and(|character| character.is_alphabetic())
                };
                if is_letter(index.checked_sub(1)) && is_letter(Some(index + 1)) {
                    RIGHT_SINGLE_CURLY_QUOTE
                } else if is_opening_context(&characters, index) {
                    LEFT_SINGLE_CURLY_QUOTE
                } else {
                    RIGHT_SINGLE_CURLY_QUOTE
                }
            }
            other => other,
        })
        .collect()
}

fn is_opening_context(characters: &[char], index: usize) -> bool {
    index == 0
        || matches!(
            characters[index - 1],
            ' ' | '\t' | '\n' | '\r' | '(' | '[' | '{' | '\u{2014}' | '\u{2013}'
        )
}

/// Claude Code `applyEditToFile`: deleting text that ends a line also deletes that line break.
fn apply_edit(content: &str, old_string: &str, new_string: &str, replace_all: bool) -> String {
    let replace = |search: &str| {
        if replace_all {
            content.replace(search, new_string)
        } else {
            content.replacen(search, new_string, 1)
        }
    };
    let with_line_break = format!("{old_string}\n");
    if new_string.is_empty() && !old_string.ends_with('\n') && content.contains(&with_line_break) {
        replace(&with_line_break)
    } else {
        replace(old_string)
    }
}

pub(super) fn take_review(operation: &mut PreparedWrite) -> Box<FileChangeReview> {
    operation
        .review
        .take()
        .expect("bound write operation must contain a file change review")
}

/// Writes a temporary file in the target directory first, then publishes it atomically: a hard link
/// for a new file, a rename for an existing one. File identity is checked before and after; if the
/// file was changed externally after approval, the write fails with a conflict.
///
/// A new file also gets its missing parent directories. On success the written content is
/// returned as a full read, so later changes need no new `Read`.
pub(super) fn execute(
    path: &Path,
    operation: PreparedWrite,
    operation_id: uuid::Uuid,
    cancel: &CancellationToken,
) -> Result<(ToolResult, FileReadRecord), ToolError> {
    let text = operation.content.as_deref().ok_or_else(|| {
        ToolError::InvalidInput("write was not bound to its target; use Tools::prepare".into())
    })?;
    let content = text.as_bytes();
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::InvalidInput("path has no parent".into()))?;
    let directory = if operation.expected.is_none() {
        open_without_following_links(
            parent,
            OpenTarget::Directory {
                create_mode: Some(NEW_DIRECTORY_MODE),
            },
        )
        .map_err(|error| io_error(parent, error))?
    } else {
        open_existing(parent, true)?
    };
    let name = CString::new(path.file_name().unwrap().as_bytes())
        .map_err(|_| ToolError::InvalidInput("path contains NUL".into()))?;
    let current = read_current(directory.as_raw_fd(), &name, path)?;
    if current
        .as_ref()
        .map(|(_, metadata)| FileIdentity::from_metadata(metadata))
        != operation.expected
    {
        return Err(ToolError::Conflict(path.into()));
    }
    let mode = current
        .as_ref()
        .map(|(_, metadata)| metadata.mode() & PRESERVED_PERMISSION_BITS)
        .unwrap_or(PRIVATE_FILE_MODE);
    let temporary = CString::new(format!(".tickloop-write-{operation_id}")).unwrap();
    // SAFETY: The directory descriptor is valid and the temporary file name is NUL-terminated; with
    // O_CREAT the mode argument is passed as required.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            PRIVATE_FILE_MODE as libc::c_uint,
        )
    };
    if descriptor < 0 {
        return Err(io_error(path, std::io::Error::last_os_error()));
    }
    // SAFETY: The descriptor returned by a successful openat is not held by anything else;
    // ownership passes to `File`.
    let mut file = unsafe { File::from_raw_fd(descriptor) };
    let result = (|| {
        file.write_all(content)
            .map_err(|error| io_error(path, error))?;
        // SAFETY: The descriptor is valid while `file` is alive.
        if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } != 0 {
            return Err(io_error(path, std::io::Error::last_os_error()));
        }
        file.sync_all().map_err(|error| io_error(path, error))?;
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let current = read_current(directory.as_raw_fd(), &name, path)?;
        if current
            .as_ref()
            .map(|(_, metadata)| FileIdentity::from_metadata(metadata))
            != operation.expected
        {
            return Err(ToolError::Conflict(path.into()));
        }
        #[cfg(test)]
        fail_at(WriteFailurePoint::BeforePublish, path)?;
        let published = if operation.expected.is_none() {
            // A new target must not replace a file that appeared only after approval: the hard link
            // fails when the target already exists.
            // SAFETY: Both names are NUL-terminated and under the same valid directory descriptor;
            // with flags 0 symlinks are not followed.
            unsafe {
                libc::linkat(
                    directory.as_raw_fd(),
                    temporary.as_ptr(),
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    0,
                )
            }
        } else {
            // SAFETY: Both names are NUL-terminated and under the same valid directory descriptor.
            unsafe {
                libc::renameat(
                    directory.as_raw_fd(),
                    temporary.as_ptr(),
                    directory.as_raw_fd(),
                    name.as_ptr(),
                )
            }
        };
        if published != 0 {
            return Err(io_error(path, std::io::Error::last_os_error()));
        }
        directory
            .sync_all()
            .map_err(|error| io_error(parent, error))?;
        // The staged descriptor is the published file, so its time is ours even if the name is
        // replaced again right away.
        let modified = file
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map_err(|error| io_error(path, error))?;
        Ok((
            ToolResult {
                content: success_message(operation.tool, path, operation.expected.is_none()),
                is_error: false,
                output_ref: None,
            },
            FileReadRecord::whole_file(text, modified),
        ))
    })();
    // SAFETY: The directory descriptor is valid and the temporary file name is NUL-terminated;
    // after a successful publish, what is removed is the leftover temporary name.
    unsafe { libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0) };
    result
}

/// Claude Code's success texts (`mapToolResultToToolResultBlockParam` of `FileWriteTool` and
/// `FileEditTool`); only `Write` tells a new file apart.
fn success_message(tool: FileChangeTool, path: &Path, is_new_file: bool) -> String {
    let path = path.display();
    match tool {
        FileChangeTool::Write if is_new_file => format!("File created successfully at: {path}"),
        FileChangeTool::Edit { replace_all: true } => {
            format!("The file {path} has been updated. All occurrences were successfully replaced.")
        }
        FileChangeTool::Write | FileChangeTool::Edit { replace_all: false } => {
            format!("The file {path} has been updated successfully.")
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq)]
enum WriteFailurePoint {
    BeforePublish,
}

#[cfg(test)]
thread_local! {
    static WRITE_FAILURE: std::cell::Cell<Option<WriteFailurePoint>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn fail_at(point: WriteFailurePoint, path: &Path) -> Result<(), ToolError> {
    if WRITE_FAILURE.with(|failure| failure.get()) == Some(point) {
        Err(io_error(
            path,
            std::io::Error::other("injected publication failure"),
        ))
    } else {
        Ok(())
    }
}

fn read_for_review(path: &Path) -> Result<Option<(String, Metadata)>, ToolError> {
    let mut file = match open_existing(path, false) {
        Ok(file) => file,
        Err(ToolError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    let before_metadata = file.metadata().map_err(|error| io_error(path, error))?;
    if !before_metadata.is_file() {
        return Err(ToolError::UnsupportedType(path.into()));
    }
    if before_metadata.len() > MAX_WRITE_BYTES as u64 {
        return Err(ToolError::InvalidInput(
            "existing file exceeds 8 MiB review limit".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(before_metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_WRITE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(path, error))?;
    let after_metadata = file.metadata().map_err(|error| io_error(path, error))?;
    let before_identity = FileIdentity::from_metadata(&before_metadata);
    let after_identity = FileIdentity::from_metadata(&after_metadata);
    if before_identity != after_identity || bytes.len() as u64 != before_metadata.len() {
        return Err(ToolError::Conflict(path.into()));
    }
    let content = String::from_utf8(bytes)
        .map_err(|_| ToolError::InvalidInput("existing file is not UTF-8 text".into()))?;
    Ok(Some((content, before_metadata)))
}

fn read_current(
    directory: libc::c_int,
    name: &CString,
    path: &Path,
) -> Result<Option<(File, Metadata)>, ToolError> {
    // SAFETY: The caller guarantees the directory descriptor is valid and the name is
    // NUL-terminated.
    let descriptor = unsafe {
        libc::openat(
            directory,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(io_error(path, error));
    }
    // SAFETY: The descriptor returned by a successful openat is not held by anything else;
    // ownership passes to `File`.
    let file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file.metadata().map_err(|error| io_error(path, error))?;
    if !metadata.is_file() {
        return Err(ToolError::UnsupportedType(path.into()));
    }
    Ok(Some((file, metadata)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Binds a `Write` of `content` as a Chat that has just read the current file would.
    fn prepared(path: &Path, content: &str) -> PreparedWrite {
        let (_, Operation::Write(operation), _) = prepare_input(
            WRITE_TOOL_NAME,
            &json!({"file_path": path,"content":content}),
        )
        .unwrap() else {
            unreachable!()
        };
        let reads = FileReadState::default();
        if let Ok(current) = fs::read_to_string(path) {
            let modified = fs::metadata(path).unwrap().modified().unwrap();
            reads.record(
                path.to_path_buf(),
                FileReadRecord::whole_file(&current, modified),
            );
        }
        bind(path, *operation, &reads, path.parent().unwrap()).unwrap()
    }

    #[test]
    fn failure_before_publish_preserves_the_original_and_removes_the_staged_file() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().canonicalize().unwrap().join("target");
        fs::write(&target, "old").unwrap();
        let operation = prepared(&target, "new");
        WRITE_FAILURE.with(|point| point.set(Some(WriteFailurePoint::BeforePublish)));
        let result = execute(
            &target,
            operation,
            uuid::Uuid::new_v4(),
            &CancellationToken::new(),
        );
        WRITE_FAILURE.with(|point| point.set(None));
        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_new_file_never_replaces_a_file_created_after_approval() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().canonicalize().unwrap().join("target");
        let operation = prepared(&target, "agent content");
        fs::write(&target, "created meanwhile").unwrap();
        let result = execute(
            &target,
            operation,
            uuid::Uuid::new_v4(),
            &CancellationToken::new(),
        );
        assert!(matches!(result, Err(ToolError::Conflict(_))));
        assert_eq!(fs::read(&target).unwrap(), b"created meanwhile");
        let operation = prepared(&target, "replacement");
        execute(
            &target,
            operation,
            uuid::Uuid::new_v4(),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"replacement");
    }
}

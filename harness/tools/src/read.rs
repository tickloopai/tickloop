//! Claude Code's `Read` tool for UTF-8 text (`FileReadTool.ts`, `utils/readFileInRange.ts`,
//! `utils/file.ts` `addLineNumbers`) and the per-Chat record of what each read returned.

use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, Read, Seek, SeekFrom},
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::SystemTime,
};

use serde::{Deserialize, Deserializer, de::Error as _};
use serde_json::{Value, json};

use super::{
    FileOutcome, Operation, ToolDefinition, ToolError, format_file_size, io_error, open_existing,
    shell::BASH_TOOL_NAME,
};
use tickloop_harness_base::image;
use tickloop_harness_protocol::{FileReadRecord, ToolResult};

pub const READ_TOOL_NAME: &str = "Read";

const DEFAULT_LINE_LIMIT: usize = 2000;
/// Claude Code `MAX_OUTPUT_SIZE`: the largest file read without an explicit `limit`.
const MAX_FILE_BYTES_WITHOUT_LIMIT: u64 = 256 * 1024;
/// Claude Code `DEFAULT_MAX_OUTPUT_TOKENS`, estimated here at four bytes per token.
const MAX_OUTPUT_TOKENS: usize = 25_000;
const BYTES_PER_TOKEN: usize = 4;
const MAX_OUTPUT_BYTES: usize = MAX_OUTPUT_TOKENS * BYTES_PER_TOKEN;
/// Normalizing `\r\n` and dropping the terminator after the last selected line remove at most
/// one byte per line plus one, so a selection larger than this can never fit the output budget.
const MAX_SCANNED_BYTES: usize = 2 * MAX_OUTPUT_BYTES + 2;
const READ_BUFFER_BYTES: usize = 64 * 1024;
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
const LINE_NUMBER_WIDTH: usize = 6;
const EMPTY_FILE_REMINDER: &str =
    "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>";
const NARROW_READ_ADVICE: &str = "Use offset and limit parameters to read specific portions of the file, or search for specific content instead of reading the whole file.";
/// Claude Code `READ_FILE_STATE_CACHE_SIZE` and its default cache byte budget.
const MAX_RECORDED_FILES: usize = 100;
const MAX_RECORDED_BYTES: usize = 25 * 1024 * 1024;
const IMAGE_SIGNATURE_BYTES: u64 = 12;

pub(super) fn definition() -> ToolDefinition {
    ToolDefinition {
        name: READ_TOOL_NAME.into(),
        description: format!(
            "Reads a file from the local filesystem. You can access any file directly by using this tool.
Assume this tool is able to read all files on the machine. If the User provides a path to a file assume that path is valid. It is okay to read a file that does not exist; an error will be returned.

Usage:
- The file_path parameter must be an absolute path, not a relative path
- By default, it reads up to {DEFAULT_LINE_LIMIT} lines starting from the beginning of the file
- You can optionally specify a line offset and limit (especially handy for long files), but it's recommended to read the whole file by not providing these parameters
- Results are returned using cat -n format, with line numbers starting at 1
- This tool allows you to read images (PNG, JPEG, GIF or WebP). When reading an image file the contents are presented visually, as you are a multimodal LLM.
- This tool can only read files, not directories. To read a directory, use an ls command via the {BASH_TOOL_NAME} tool.
- You will regularly be asked to read screenshots. If the user provides a path to a screenshot, ALWAYS use this tool to view the file at the path. This tool will work with all temporary file paths.
- If you read a file that exists but has empty contents you will receive a system reminder warning in place of file contents."
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to read"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "The line number to start reading from. Only provide if the file is too large to read at once"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "The number of lines to read. Only provide if the file is too large to read at once."
                }
            },
            "required": ["file_path"],
            "additionalProperties": false
        }),
    }
}

/// The lines a `Read` call asked for, as the model supplied them.
#[derive(Clone, Debug)]
pub(super) struct ReadRequest {
    offset: Option<usize>,
    limit: Option<usize>,
}

pub(super) fn prepare_input(
    _: &str,
    input: &Value,
) -> Result<(String, Operation, Value), ToolError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ReadInput {
        file_path: String,
        #[serde(default, deserialize_with = "semantic_integer")]
        offset: Option<usize>,
        #[serde(default, deserialize_with = "semantic_integer")]
        limit: Option<usize>,
    }
    let parsed: ReadInput = serde_json::from_value(input.clone())
        .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    if parsed.limit == Some(0) {
        return Err(ToolError::InvalidInput(
            "limit must be a positive number of lines".into(),
        ));
    }
    let mut normalized_input = json!({});
    if let Some(offset) = parsed.offset {
        normalized_input["offset"] = json!(offset);
    }
    if let Some(limit) = parsed.limit {
        normalized_input["limit"] = json!(limit);
    }
    Ok((
        parsed.file_path,
        Operation::Read(ReadRequest {
            offset: parsed.offset,
            limit: parsed.limit,
        }),
        normalized_input,
    ))
}

/// Claude Code `semanticNumber`: models occasionally quote numbers, so digit strings count too.
pub(super) fn semantic_integer<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<usize>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SemanticInteger {
        Number(usize),
        Text(String),
    }
    match SemanticInteger::deserialize(deserializer)? {
        SemanticInteger::Number(value) => Ok(Some(value)),
        SemanticInteger::Text(text) => text.parse().map(Some).map_err(|_| {
            D::Error::custom(format!("expected a non-negative integer, found {text:?}"))
        }),
    }
}

/// Reads text as numbered lines, or an image for the model to view when `accepts_images`.
pub(super) fn execute(
    path: &Path,
    request: &ReadRequest,
    accepts_images: bool,
) -> Result<FileOutcome, ToolError> {
    let mut file = open_existing(path, false)?;
    let metadata = file.metadata().map_err(|error| io_error(path, error))?;
    if metadata.is_dir() {
        return Err(ToolError::Rejected(format!(
            "EISDIR: illegal operation on a directory, read '{}'",
            path.display()
        )));
    }
    if !metadata.is_file() {
        return Err(ToolError::UnsupportedType(path.into()));
    }
    let mut signature = Vec::new();
    (&mut file)
        .take(IMAGE_SIGNATURE_BYTES)
        .read_to_end(&mut signature)
        .map_err(|error| io_error(path, error))?;
    if image::is_supported_image(&signature) {
        return read_image(file, path, metadata.len(), accepts_images);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io_error(path, error))?;
    read_text(file, path, &metadata, request).map(FileOutcome::from)
}

fn read_text(
    file: File,
    path: &Path,
    metadata: &std::fs::Metadata,
    request: &ReadRequest,
) -> Result<(ToolResult, FileReadRecord), ToolError> {
    if request.limit.is_none() && metadata.len() > MAX_FILE_BYTES_WITHOUT_LIMIT {
        return Err(ToolError::Rejected(format!(
            "File content ({}) exceeds maximum allowed size ({}). {NARROW_READ_ADVICE}",
            format_file_size(usize::try_from(metadata.len()).unwrap_or(usize::MAX)),
            format_file_size(MAX_FILE_BYTES_WITHOUT_LIMIT as usize),
        )));
    }
    let modified = metadata.modified().map_err(|error| io_error(path, error))?;
    let start_line = request.offset.unwrap_or(1).max(1);
    let first_index = start_line - 1;
    let selection =
        first_index..first_index.saturating_add(request.limit.unwrap_or(DEFAULT_LINE_LIMIT));
    let scanned = scan_lines(
        &mut std::io::BufReader::with_capacity(READ_BUFFER_BYTES, file),
        &selection,
    )
    .map_err(|error| io_error(path, error))?;
    if scanned.selected.contains(&0) {
        return Err(binary_file_error(path));
    }
    if scanned.selected_bytes > MAX_SCANNED_BYTES {
        return Err(token_budget_error(scanned.selected_bytes));
    }
    let content = String::from_utf8(normalized_lines(&scanned.selected, scanned.line_count))
        .map_err(|_| {
            ToolError::Rejected(
                "This tool can only read UTF-8 text. The file contains bytes that are not valid UTF-8."
                    .into(),
            )
        })?;
    if content.len() > MAX_OUTPUT_BYTES {
        return Err(token_budget_error(content.len()));
    }
    let text = if !content.is_empty() {
        add_line_numbers(&content, start_line)
    } else if scanned.total_lines == 0 {
        EMPTY_FILE_REMINDER.into()
    } else {
        format!(
            "<system-reminder>Warning: the file exists but is shorter than the provided offset ({start_line}). The file has {} lines.</system-reminder>",
            scanned.total_lines
        )
    };
    let record = FileReadRecord {
        is_full_read: first_index == 0 && scanned.total_lines <= selection.end,
        content,
        modified,
        offset: start_line,
        limit: request.limit,
    };
    Ok((
        ToolResult {
            content: text,
            is_error: false,
            output_ref: None,
        },
        record,
    ))
}

/// Like Claude Code `Read`: the model receives the whole prepared image, while the text result
/// only summarizes it. A model without image input gets an explanation it can relay instead,
/// as a successful result so the run continues, and the image is never loaded.
fn read_image(
    mut file: File,
    path: &Path,
    total_bytes: u64,
    accepts_images: bool,
) -> Result<FileOutcome, ToolError> {
    if !accepts_images {
        return Ok(ToolResult {
            content: format!(
                "ERROR: Cannot read image file {} (this model does not support image input). Inform the user.",
                path.display()
            ),
            is_error: false,
            output_ref: None,
        }
        .into());
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io_error(path, error))?;
    let mut bytes = Vec::new();
    file.take(image::MAX_SOURCE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(path, error))?;
    let image = image::prepare(&bytes).map_err(|source| ToolError::Image {
        path: path.into(),
        source,
    })?;
    let summary = json!({
        "source": path,
        "media_type": image.media_type.mime_type(),
        "width": image.width,
        "height": image.height,
        "total_bytes": total_bytes,
    });
    Ok(FileOutcome {
        result: ToolResult {
            content: summary.to_string(),
            is_error: false,
            output_ref: None,
        },
        read: None,
        images: vec![image],
    })
}

fn binary_file_error(path: &Path) -> ToolError {
    let kind = path.extension().map_or_else(
        || "binary".into(),
        |extension| format!("binary .{}", extension.to_string_lossy()),
    );
    ToolError::Rejected(format!(
        "This tool cannot read binary files. The file appears to be a {kind} file. Please use appropriate tools for binary file analysis."
    ))
}

fn token_budget_error(content_bytes: usize) -> ToolError {
    ToolError::Rejected(format!(
        "File content ({} tokens) exceeds maximum allowed tokens ({MAX_OUTPUT_TOKENS}). {NARROW_READ_ADVICE}",
        content_bytes.div_ceil(BYTES_PER_TOKEN)
    ))
}

struct ScannedLines {
    /// Raw bytes of the selected lines with their terminators, kept up to `MAX_SCANNED_BYTES`.
    selected: Vec<u8>,
    /// Size of the whole selection, including bytes past the kept prefix.
    selected_bytes: usize,
    line_count: usize,
    total_lines: usize,
}

/// Splits like Claude Code `readFileInRange`: a leading BOM is dropped, every `\n` ends a line,
/// and the fragment after the last `\n` is one more line, so only an empty file has no lines.
fn scan_lines(
    reader: &mut impl BufRead,
    selection: &Range<usize>,
) -> std::io::Result<ScannedLines> {
    if reader.fill_buf()?.starts_with(UTF8_BOM) {
        reader.consume(UTF8_BOM.len());
    }
    let mut selected = Vec::new();
    let mut selected_bytes = 0;
    let mut line_index = 0;
    let mut has_content = false;
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        has_content = true;
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        let piece_length = newline.map_or(chunk.len(), |index| index + 1);
        if selection.contains(&line_index) {
            selected_bytes += piece_length;
            if selected_bytes <= MAX_SCANNED_BYTES {
                selected.extend_from_slice(&chunk[..piece_length]);
            }
        }
        reader.consume(piece_length);
        if newline.is_some() {
            line_index += 1;
        }
    }
    let total_lines = if has_content { line_index + 1 } else { 0 };
    Ok(ScannedLines {
        selected,
        selected_bytes,
        line_count: total_lines
            .min(selection.end)
            .saturating_sub(selection.start),
        total_lines,
    })
}

fn normalized_lines(selected: &[u8], line_count: usize) -> Vec<u8> {
    selected
        .split(|&byte| byte == b'\n')
        .take(line_count)
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .collect::<Vec<_>>()
        .join(&b'\n')
}

/// Claude Code `addLineNumbers` in its padded-arrow form.
fn add_line_numbers(content: &str, start_line: usize) -> String {
    content
        .split('\n')
        .enumerate()
        .map(|(index, line)| format!("{:>LINE_NUMBER_WIDTH$}\u{2192}{line}", start_line + index))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Building a [`FileReadRecord`] in the form `Read` records; the record lives in the protocol crate.
pub trait FileReadRecordExt {
    /// A view of a whole file's `text`, recorded in the form `Read` returns its lines.
    fn whole_file(text: &str, modified: SystemTime) -> Self;
}

impl FileReadRecordExt for FileReadRecord {
    fn whole_file(text: &str, modified: SystemTime) -> Self {
        Self {
            content: recorded_text(text),
            modified,
            offset: 1,
            limit: None,
            is_full_read: true,
        }
    }
}

/// `text` as a full `Read` records it: without a leading BOM and with each line's `\r` dropped.
pub(super) fn recorded_text(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One Chat's in-memory reads keyed by resolved absolute path; like Claude Code's LRU cache it
/// keeps the most recently used 100 files within 25 MB of content.
#[derive(Clone, Default)]
pub struct FileReadState {
    reads: Arc<Mutex<RecentReads>>,
}

#[derive(Default)]
struct RecentReads {
    entries: HashMap<PathBuf, RecentRead>,
    content_bytes: usize,
    clock: u64,
}

struct RecentRead {
    last_used: u64,
    record: FileReadRecord,
}

impl FileReadState {
    pub fn upgrade_checkpoint(&self) -> Vec<(PathBuf, FileReadRecord)> {
        let reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
        let mut entries = reads.entries.iter().collect::<Vec<_>>();
        entries.sort_by_key(|(_, entry)| entry.last_used);
        entries
            .into_iter()
            .map(|(path, entry)| (path.clone(), entry.record.clone()))
            .collect()
    }

    pub fn get(&self, path: &Path) -> Option<FileReadRecord> {
        let mut reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
        reads.clock += 1;
        let clock = reads.clock;
        let entry = reads.entries.get_mut(path)?;
        entry.last_used = clock;
        Some(entry.record.clone())
    }

    pub fn record(&self, path: PathBuf, record: FileReadRecord) {
        let mut reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
        reads.clock += 1;
        reads.content_bytes += record.content.len();
        let entry = RecentRead {
            last_used: reads.clock,
            record,
        };
        if let Some(replaced) = reads.entries.insert(path, entry) {
            reads.content_bytes -= replaced.record.content.len();
        }
        while reads.entries.len() > MAX_RECORDED_FILES || reads.content_bytes > MAX_RECORDED_BYTES {
            let Some(least_recent) = reads
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            if let Some(evicted) = reads.entries.remove(&least_recent) {
                reads.content_bytes -= evicted.record.content.len();
            }
        }
    }

    /// Forgets every read, as Claude Code does once compaction drops the file contents from the
    /// conversation.
    pub fn clear(&self) {
        let mut reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
        reads.entries.clear();
        reads.content_bytes = 0;
    }
}

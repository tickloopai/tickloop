//! Claude Code's `Read` tool for UTF-8 text (`FileReadTool.ts`, `utils/readFileInRange.ts`,
//! `utils/file.ts` `addLineNumbers`) and the per-Chat record of what each read returned.

use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::SystemTime,
};

use serde::{Deserialize, Deserializer, de::Error as _};
use serde_json::{Value, json};

use super::{
    FileOutcome, Operation, ToolDefinition, ToolError,
    chat_files::{FileTarget, tool_error},
    file_port::{FileContent, FileInspection, FileMetadata, FileQuery, FileVersion, error_code},
    format_file_size,
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
/// Bytes asked of the file per read: one read takes in any file small enough to read without a
/// `limit`.
const READ_CHUNK_BYTES: usize = 1024 * 1024;
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
const LINE_NUMBER_WIDTH: usize = 6;
const EMPTY_FILE_REMINDER: &str =
    "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>";
const NARROW_READ_ADVICE: &str = "Use offset and limit parameters to read specific portions of the file, or search for specific content instead of reading the whole file.";
/// Claude Code `READ_FILE_STATE_CACHE_SIZE` and its default cache byte budget.
const MAX_RECORDED_FILES: usize = 100;
const MAX_RECORDED_BYTES: usize = 25 * 1024 * 1024;
const IMAGE_SIGNATURE_BYTES: usize = 12;

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
pub(super) async fn execute(
    target: &FileTarget,
    request: &ReadRequest,
    accepts_images: bool,
) -> Result<FileOutcome, ToolError> {
    let (mut chunks, first) = FileChunks::open(target).await?;
    let signature = &first[..first.len().min(IMAGE_SIGNATURE_BYTES)];
    if image::is_supported_image(signature) {
        return read_image(chunks, first, &target.shown, accepts_images).await;
    }
    let version = chunks.metadata.version.clone();
    let (result, record) = read_text(&mut chunks, first, &target.shown, request).await?;
    Ok(FileOutcome {
        result,
        read: Some((record, version)),
        images: Vec::new(),
    })
}

/// A file read through its host chunk by chunk; every chunk must come from the version the first
/// one did.
struct FileChunks<'t> {
    target: &'t FileTarget,
    metadata: FileMetadata,
    offset: u64,
}

impl<'t> FileChunks<'t> {
    async fn open(target: &'t FileTarget) -> Result<(Self, Vec<u8>), ToolError> {
        let content = read_chunk(target, 0, READ_CHUNK_BYTES).await?;
        Ok((
            Self {
                target,
                offset: content.data.len() as u64,
                metadata: content.metadata,
            },
            content.data,
        ))
    }

    async fn next(&mut self) -> Result<Option<Vec<u8>>, ToolError> {
        if self.offset >= self.metadata.len {
            return Ok(None);
        }
        let content = read_chunk(self.target, self.offset, READ_CHUNK_BYTES).await?;
        if content.metadata.version != self.metadata.version {
            return Err(ToolError::Conflict(self.target.shown.clone()));
        }
        if content.data.is_empty() {
            return Ok(None);
        }
        self.offset += content.data.len() as u64;
        Ok(Some(content.data))
    }
}

/// `limit` bytes of the file from `offset`, with Claude Code's answer for a directory.
pub(super) async fn read_chunk(
    target: &FileTarget,
    offset: u64,
    limit: usize,
) -> Result<FileContent, ToolError> {
    let inspection = target
        .host
        .inspect(FileQuery::Read {
            path: target.path.clone(),
            offset,
            limit,
        })
        .await
        .map_err(|error| match error.code.as_str() {
            error_code::IS_DIRECTORY => ToolError::Rejected(format!(
                "EISDIR: illegal operation on a directory, read '{}'",
                target.shown.display()
            )),
            _ => tool_error(error, &target.shown),
        })?;
    match inspection {
        FileInspection::Read(content) => Ok(content),
        _ => Err(ToolError::Io {
            path: target.shown.clone(),
            reason: "the environment answered a read with something else".into(),
        }),
    }
}

async fn read_text(
    chunks: &mut FileChunks<'_>,
    first: Vec<u8>,
    path: &Path,
    request: &ReadRequest,
) -> Result<(ToolResult, FileReadRecord), ToolError> {
    let total_bytes = chunks.metadata.len;
    if request.limit.is_none() && total_bytes > MAX_FILE_BYTES_WITHOUT_LIMIT {
        return Err(ToolError::Rejected(format!(
            "File content ({}) exceeds maximum allowed size ({}). {NARROW_READ_ADVICE}",
            format_file_size(usize::try_from(total_bytes).unwrap_or(usize::MAX)),
            format_file_size(MAX_FILE_BYTES_WITHOUT_LIMIT as usize),
        )));
    }
    let modified = chunks.metadata.modified.unwrap_or(SystemTime::UNIX_EPOCH);
    let start_line = request.offset.unwrap_or(1).max(1);
    let first_index = start_line - 1;
    let selection =
        first_index..first_index.saturating_add(request.limit.unwrap_or(DEFAULT_LINE_LIMIT));
    let mut scanner = LineScanner::new(selection.clone());
    scanner.feed(first.strip_prefix(UTF8_BOM).unwrap_or(&first));
    while let Some(chunk) = chunks.next().await? {
        scanner.feed(&chunk);
    }
    let scanned = scanner.finish();
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
async fn read_image(
    mut chunks: FileChunks<'_>,
    mut bytes: Vec<u8>,
    path: &Path,
    accepts_images: bool,
) -> Result<FileOutcome, ToolError> {
    let total_bytes = chunks.metadata.len;
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
    while bytes.len() <= image::MAX_SOURCE_BYTES
        && let Some(chunk) = chunks.next().await?
    {
        bytes.extend_from_slice(&chunk);
    }
    bytes.truncate(image::MAX_SOURCE_BYTES + 1);
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

/// Splits like Claude Code `readFileInRange`, fed the file after a leading BOM: every `\n` ends a
/// line, and the fragment after the last `\n` is one more line, so only an empty file has no
/// lines.
struct LineScanner {
    selection: Range<usize>,
    selected: Vec<u8>,
    selected_bytes: usize,
    line_index: usize,
    has_content: bool,
}

impl LineScanner {
    fn new(selection: Range<usize>) -> Self {
        Self {
            selection,
            selected: Vec::new(),
            selected_bytes: 0,
            line_index: 0,
            has_content: false,
        }
    }

    fn feed(&mut self, mut chunk: &[u8]) {
        while !chunk.is_empty() {
            self.has_content = true;
            let newline = chunk.iter().position(|&byte| byte == b'\n');
            let piece_length = newline.map_or(chunk.len(), |index| index + 1);
            if self.selection.contains(&self.line_index) {
                self.selected_bytes += piece_length;
                if self.selected_bytes <= MAX_SCANNED_BYTES {
                    self.selected.extend_from_slice(&chunk[..piece_length]);
                }
            }
            chunk = &chunk[piece_length..];
            if newline.is_some() {
                self.line_index += 1;
            }
        }
    }

    fn finish(self) -> ScannedLines {
        let total_lines = if self.has_content {
            self.line_index + 1
        } else {
            0
        };
        ScannedLines {
            selected: self.selected,
            selected_bytes: self.selected_bytes,
            line_count: total_lines
                .min(self.selection.end)
                .saturating_sub(self.selection.start),
            total_lines,
        }
    }
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

/// One Chat's in-memory reads keyed by the absolute path the model sees for the file once links
/// are resolved, with the version the environment reported; like Claude Code's LRU cache it keeps
/// the most recently used 100 files within 25 MB of content.
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
    version: Option<FileVersion>,
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
        self.get_with_version(path).map(|(record, _)| record)
    }

    /// The read of `path` and the version the file had then, when the environment reported one.
    pub(crate) fn get_with_version(
        &self,
        path: &Path,
    ) -> Option<(FileReadRecord, Option<FileVersion>)> {
        let mut reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
        reads.clock += 1;
        let clock = reads.clock;
        let entry = reads.entries.get_mut(path)?;
        entry.last_used = clock;
        Some((entry.record.clone(), entry.version.clone()))
    }

    pub fn record(&self, path: PathBuf, record: FileReadRecord) {
        self.record_version(path, record, None);
    }

    pub(crate) fn record_version(
        &self,
        path: PathBuf,
        record: FileReadRecord,
        version: Option<FileVersion>,
    ) {
        let mut reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
        reads.clock += 1;
        reads.content_bytes += record.content.len();
        let entry = RecentRead {
            last_used: reads.clock,
            record,
            version,
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

    /// Forgets the read of `path`, whose content is no longer known.
    pub(crate) fn forget(&self, path: &Path) {
        let mut reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(forgotten) = reads.entries.remove(path) {
            reads.content_bytes -= forgotten.record.content.len();
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

use std::{
    fs,
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
pub use tickloop_agent_policy::loop_skill::*;
const LOOP_FILE_NAME: &str = "loop.md";
const PROJECT_LOOP_FILE: &str = ".tickloop/loop.md";
/// Reading this far past the limit is enough to tell a file was cut off.
const READ_PAST_LIMIT_BYTES: u64 = 4;

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

impl LoopInstructionSource for LoopFiles {
    fn instructions(&self) -> Result<LoopInstructions, LoopFileError> {
        LoopFiles::instructions(self)
    }
}

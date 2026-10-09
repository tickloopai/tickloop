use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};
use tickloop_agent_protocol::{
    FileChangeAction, FileChangeArtifactRef, FileChangeReviewSummary, FileIdentity, FileVersion,
    PreparedFileChange,
};

pub const FILE_CHANGE_REVIEW_VERSION: u32 = 1;
const DIFF_CONTEXT_LINES: usize = 3;
pub(crate) const ARTIFACT_DIRECTORY: &str = "file-changes";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileChangeLine {
    pub kind: FileChangeLineKind,
    pub old_line: Option<u64>,
    pub new_line: Option<u64>,
    pub text: String,
    pub has_newline: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileChangeHunk {
    pub old_start: u64,
    pub old_len: u64,
    pub new_start: u64,
    pub new_len: u64,
    pub lines: Vec<FileChangeLine>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileChangeReview {
    pub version: u32,
    pub action: FileChangeAction,
    pub path: PathBuf,
    pub before: FileVersion,
    pub after: FileVersion,
    pub hunks: Vec<FileChangeHunk>,
}

#[derive(Debug, thiserror::Error)]
pub enum FileChangeArtifactError {
    #[error("file change review artifact is missing: {0}")]
    Missing(PathBuf),
    #[error("file change review artifact is corrupt: {path}: {reason}")]
    Corrupt { path: PathBuf, reason: String },
    #[error("file change review artifact access failed for {path}: {reason}")]
    Io { path: PathBuf, reason: String },
}

impl FileChangeReview {
    pub fn summary(&self) -> FileChangeReviewSummary {
        let (added_lines, removed_lines) = self.hunks.iter().flat_map(|hunk| &hunk.lines).fold(
            (0, 0),
            |(added, removed), line| match line.kind {
                FileChangeLineKind::Added => (added + 1, removed),
                FileChangeLineKind::Removed => (added, removed + 1),
                FileChangeLineKind::Context => (added, removed),
            },
        );
        FileChangeReviewSummary {
            action: self.action,
            path: self.path.clone(),
            before: self.before.clone(),
            after: self.after.clone(),
            added_lines,
            removed_lines,
            hunk_count: self.hunks.len() as u64,
        }
    }
}

pub fn load_file_change_review(
    reference: &FileChangeArtifactRef,
) -> Result<FileChangeReview, FileChangeArtifactError> {
    let metadata = match std::fs::metadata(&reference.path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(FileChangeArtifactError::Missing(reference.path.clone()));
        }
        Err(error) => {
            return Err(FileChangeArtifactError::Io {
                path: reference.path.clone(),
                reason: error.to_string(),
            });
        }
    };
    if metadata.len() != reference.byte_len {
        return Err(FileChangeArtifactError::Corrupt {
            path: reference.path.clone(),
            reason: format!(
                "expected {} bytes but found {}",
                reference.byte_len,
                metadata.len()
            ),
        });
    }
    if !metadata.is_file() {
        return Err(FileChangeArtifactError::Corrupt {
            path: reference.path.clone(),
            reason: "artifact is not a regular file".into(),
        });
    }
    let bytes = match std::fs::read(&reference.path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(FileChangeArtifactError::Missing(reference.path.clone()));
        }
        Err(error) => {
            return Err(FileChangeArtifactError::Io {
                path: reference.path.clone(),
                reason: error.to_string(),
            });
        }
    };
    if bytes.len() as u64 != reference.byte_len {
        return Err(FileChangeArtifactError::Corrupt {
            path: reference.path.clone(),
            reason: format!(
                "expected {} bytes but found {}",
                reference.byte_len,
                bytes.len()
            ),
        });
    }
    let actual_hash = sha256(&bytes);
    if actual_hash != reference.sha256 {
        return Err(FileChangeArtifactError::Corrupt {
            path: reference.path.clone(),
            reason: format!(
                "expected SHA-256 {} but found {actual_hash}",
                reference.sha256
            ),
        });
    }
    let review = serde_json::from_slice::<FileChangeReview>(&bytes).map_err(|error| {
        FileChangeArtifactError::Corrupt {
            path: reference.path.clone(),
            reason: error.to_string(),
        }
    })?;
    if review.version != FILE_CHANGE_REVIEW_VERSION {
        return Err(FileChangeArtifactError::Corrupt {
            path: reference.path.clone(),
            reason: format!("unsupported version {}", review.version),
        });
    }
    Ok(review)
}

pub fn build_file_change_review(
    action: FileChangeAction,
    path: PathBuf,
    before: &str,
    before_exists: bool,
    before_identity: Option<FileIdentity>,
    after: &str,
    after_exists: bool,
) -> FileChangeReview {
    let diff = TextDiff::from_lines(before, after);
    let hunks = diff
        .grouped_ops(DIFF_CONTEXT_LINES)
        .into_iter()
        .map(|operations| {
            let old_len = operations
                .iter()
                .map(|operation| operation.old_range().len() as u64)
                .sum();
            let new_len = operations
                .iter()
                .map(|operation| operation.new_range().len() as u64)
                .sum();
            let old_start = operations.first().map_or(0, |operation| {
                hunk_start(operation.old_range().start, old_len)
            });
            let new_start = operations.first().map_or(0, |operation| {
                hunk_start(operation.new_range().start, new_len)
            });
            let lines = operations
                .iter()
                .flat_map(|operation| diff.iter_changes(operation))
                .map(|change| {
                    let value = change.value();
                    let has_newline = value.ends_with('\n');
                    FileChangeLine {
                        kind: match change.tag() {
                            ChangeTag::Equal => FileChangeLineKind::Context,
                            ChangeTag::Delete => FileChangeLineKind::Removed,
                            ChangeTag::Insert => FileChangeLineKind::Added,
                        },
                        old_line: change.old_index().map(|index| index as u64 + 1),
                        new_line: change.new_index().map(|index| index as u64 + 1),
                        text: value.strip_suffix('\n').unwrap_or(value).to_owned(),
                        has_newline,
                    }
                })
                .collect();
            FileChangeHunk {
                old_start,
                old_len,
                new_start,
                new_len,
                lines,
            }
        })
        .collect();
    FileChangeReview {
        version: FILE_CHANGE_REVIEW_VERSION,
        action,
        path,
        before: file_version(before, before_exists, before_identity),
        after: file_version(after, after_exists, None),
        hunks,
    }
}

fn hunk_start(zero_based_start: usize, length: u64) -> u64 {
    if length == 0 {
        zero_based_start as u64
    } else {
        zero_based_start as u64 + 1
    }
}

pub(crate) fn persist_file_change_review(
    output_dir: &Path,
    review: &FileChangeReview,
) -> Result<PreparedFileChange, FileChangeArtifactError> {
    let bytes = serde_json::to_vec(review).map_err(|error| FileChangeArtifactError::Corrupt {
        path: output_dir.to_path_buf(),
        reason: error.to_string(),
    })?;
    let artifact_dir = output_dir.join(ARTIFACT_DIRECTORY);
    std::fs::create_dir_all(&artifact_dir).map_err(|error| FileChangeArtifactError::Io {
        path: artifact_dir.clone(),
        reason: error.to_string(),
    })?;
    let artifact_id = uuid::Uuid::new_v4();
    let path = artifact_dir.join(format!("review-{artifact_id}.json"));
    let temporary_path = artifact_dir.join(format!(".review-{artifact_id}.tmp"));
    write_artifact(&temporary_path, &path, &bytes)?;
    Ok(PreparedFileChange {
        summary: review.summary(),
        artifact: FileChangeArtifactRef {
            path,
            byte_len: bytes.len() as u64,
            sha256: sha256(&bytes),
        },
    })
}

fn write_artifact(
    temporary_path: &Path,
    destination: &Path,
    bytes: &[u8],
) -> Result<(), FileChangeArtifactError> {
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(temporary_path)
            .map_err(|error| FileChangeArtifactError::Io {
                path: temporary_path.to_path_buf(),
                reason: error.to_string(),
            })?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| FileChangeArtifactError::Io {
                path: temporary_path.to_path_buf(),
                reason: error.to_string(),
            })?;
        std::fs::rename(temporary_path, destination).map_err(|error| {
            FileChangeArtifactError::Io {
                path: destination.to_path_buf(),
                reason: error.to_string(),
            }
        })?;
        File::open(destination.parent().unwrap())
            .and_then(|directory| directory.sync_all())
            .map_err(|error| FileChangeArtifactError::Io {
                path: destination.parent().unwrap().to_path_buf(),
                reason: error.to_string(),
            })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary_path);
    }
    result
}

fn file_version(content: &str, exists: bool, identity: Option<FileIdentity>) -> FileVersion {
    FileVersion {
        exists,
        identity,
        byte_len: content.len() as u64,
        line_count: content.split_inclusive('\n').count() as u64,
        ends_with_newline: content.ends_with('\n'),
        sha256: sha256(content.as_bytes()),
    }
}

fn sha256(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_preserves_every_changed_line_and_eof_state() {
        let before = "alpha\nold one\nmiddle\nold two";
        let after = "alpha\nnew one\nmiddle\nnew two\n";
        let review = build_file_change_review(
            FileChangeAction::Edit,
            PathBuf::from("notes.txt"),
            before,
            true,
            Some(FileIdentity {
                device: 1,
                inode: 2,
                byte_len: before.len() as u64,
                modified: (3, 4),
                changed: (5, 6),
            }),
            after,
            true,
        );

        let changed = review
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| line.kind != FileChangeLineKind::Context)
            .collect::<Vec<_>>();
        assert_eq!(changed.len(), 4);
        assert!(changed.iter().any(|line| line.text == "old two"));
        assert!(changed.iter().any(|line| line.text == "new two"));
        assert!(!review.before.ends_with_newline);
        assert!(review.after.ends_with_newline);
        assert!(review.before.exists);
        assert!(review.after.exists);
        assert_eq!(review.summary().added_lines, 2);
        assert_eq!(review.summary().removed_lines, 2);
    }

    #[test]
    fn separated_changes_create_hunks_with_complete_changed_ranges() {
        let before = (0..20)
            .map(|line| format!("old {line}\n"))
            .collect::<String>();
        let after = before
            .replace("old 1\n", "new 1\n")
            .replace("old 18\n", "new 18\n");
        let review = build_file_change_review(
            FileChangeAction::ReplaceAll,
            PathBuf::from("notes.txt"),
            &before,
            true,
            None,
            &after,
            true,
        );

        assert_eq!(review.hunks.len(), 2);
        assert_eq!(review.summary().added_lines, 2);
        assert_eq!(review.summary().removed_lines, 2);
    }

    #[test]
    fn sidecar_round_trip_detects_missing_and_corrupt_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let review = build_file_change_review(
            FileChangeAction::Create,
            PathBuf::from("new.txt"),
            "",
            false,
            None,
            "hello\n",
            true,
        );
        let prepared = persist_file_change_review(directory.path(), &review).unwrap();
        assert_eq!(load_file_change_review(&prepared.artifact).unwrap(), review);
        assert_eq!(
            review.before.sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(!review.before.exists);
        assert!(review.after.exists);

        let mut corrupt = std::fs::read(&prepared.artifact.path).unwrap();
        corrupt[0] ^= 1;
        std::fs::write(&prepared.artifact.path, corrupt).unwrap();
        let error = load_file_change_review(&prepared.artifact).unwrap_err();
        assert!(matches!(error, FileChangeArtifactError::Corrupt { .. }));
        assert!(error.to_string().contains("expected SHA-256"));
        std::fs::remove_file(&prepared.artifact.path).unwrap();
        assert!(matches!(
            load_file_change_review(&prepared.artifact),
            Err(FileChangeArtifactError::Missing(_))
        ));
    }
}

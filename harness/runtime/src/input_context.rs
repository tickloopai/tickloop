//! Files and directories a prompt mentions with `@`, attached to it as context. They are read
//! through the Chat's Execution Environment, from its Folder and the Additional Directories it can
//! read, wherever those are.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use tickloop_harness_base::image::{self, MAX_BASE64_BYTES};
use tickloop_harness_protocol::{
    FileReadRecord, Image, ImageContext, ImageOrigin, InputContext, TextContext,
};
use tickloop_harness_tools::FileReadRecordExt;
use tickloop_harness_tools::file_port::{
    FileCapabilities, FileContent, FileHost, FileInspection, FileKind, FilePath, FileQuery,
};

pub(super) const MAX_REFERENCES: usize = 20;
pub(super) const MAX_CONTEXTS: usize = 64;
const MAX_RESOURCE_BYTES: usize = 1024 * 1024;
const MAX_URI_BYTES: usize = 8 * 1024;
const MAX_NAME_BYTES: usize = 4 * 1024;
const MAX_MIME_BYTES: usize = 256;
const MAX_RESTORED_CONTEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 64 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 1_000;
const IMAGE_SIGNATURE_BYTES: usize = 12;
/// Image extensions recognised even when the header is unsupported, so the user gets an
/// image-format error instead of a text-encoding one.
const IMAGE_EXTENSIONS: [&str; 12] = [
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "heic", "heif", "avif", "ico",
];

/// Checks contexts restored from a saved prompt before they are attached again.
pub(super) fn restored_contexts_are_valid(contexts: &[InputContext]) -> bool {
    let mut paths = HashSet::new();
    let mut pasted_ids = HashSet::new();
    let mut attached_ids = HashSet::new();
    let mut resource_bytes = 0usize;
    let mut prompt_bytes = 0usize;
    contexts.len() <= MAX_CONTEXTS
        && contexts.iter().all(|context| {
            let within_limit = match context {
                InputContext::File(text) | InputContext::Directory(text) => {
                    text.content.len() <= MAX_RESTORED_CONTEXT_BYTES
                }
                InputContext::PromptText { text } => {
                    prompt_bytes = prompt_bytes.saturating_add(text.len());
                    prompt_bytes <= 256 * 1024
                }
                InputContext::Image(image) => image.image.data.len() <= MAX_BASE64_BYTES,
                InputContext::Resource(resource) => {
                    resource_bytes = resource_bytes
                        .saturating_add(resource.content.as_ref().map_or(0, String::len));
                    !resource.uri.trim().is_empty()
                        && resource.uri.len() <= MAX_URI_BYTES
                        && resource
                            .name
                            .as_ref()
                            .is_none_or(|name| name.len() <= MAX_NAME_BYTES)
                        && resource
                            .mime_type
                            .as_ref()
                            .is_none_or(|mime| mime.len() <= MAX_MIME_BYTES)
                        && resource_bytes <= MAX_RESOURCE_BYTES
                }
                InputContext::Later(_) => false,
            };
            let is_unique = match context {
                InputContext::Image(ImageContext {
                    origin: ImageOrigin::Pasted { id },
                    ..
                }) => pasted_ids.insert(*id),
                InputContext::Image(ImageContext {
                    origin: ImageOrigin::Attached { id, uri },
                    ..
                }) => {
                    attached_ids.insert(*id)
                        && uri.as_ref().is_none_or(|uri| uri.len() <= MAX_URI_BYTES)
                }
                // Repeated resource URIs can carry different text blocks; preserve their order.
                InputContext::Resource(_) | InputContext::PromptText { .. } => true,
                _ => context
                    .path()
                    .is_some_and(|path| path.to_str().is_some() && paths.insert(path.to_owned())),
            };
            within_limit && is_unique
        })
}

/// Refuses `path`, absolute or relative to the Folder, before anything else is done with it when
/// it lies in a directory the Chat cannot read, or outside every directory where the environment
/// reads nothing.
pub(super) fn check_readable(capabilities: &FileCapabilities, path: &Path) -> Result<(), String> {
    let readable = path
        .to_str()
        .and_then(|text| capabilities.resolve(text).ok())
        .is_some_and(|requested| capabilities.access(&requested).can_read());
    if readable {
        Ok(())
    } else {
        Err(outside_workspace(path))
    }
}

fn outside_workspace(path: &Path) -> String {
    format!(
        "referenced path is outside the configured workspace: {}",
        path.display()
    )
}

/// Attached contexts, plus a read record for each file attached whole: like Claude Code's
/// @-mentioned files, those count as read for `Write` and `Edit`.
pub(super) struct LoadedContexts {
    pub contexts: Vec<InputContext>,
    pub file_reads: Vec<(PathBuf, FileReadRecord)>,
}

/// Reads what `paths` name in the Folder and the Additional Directories the Chat can read, with
/// their links resolved; anything elsewhere is refused.
pub(super) async fn load(
    files: &dyn FileHost,
    paths: Vec<PathBuf>,
) -> Result<LoadedContexts, String> {
    if paths.len() > MAX_REFERENCES {
        return Err(format!(
            "a prompt can reference at most {MAX_REFERENCES} files or directories"
        ));
    }
    let capabilities = files.capabilities();
    let mut seen = HashSet::new();
    let mut loaded = LoadedContexts {
        contexts: Vec::with_capacity(paths.len()),
        file_reads: Vec::new(),
    };
    for path in paths {
        let Some(text) = path.to_str() else {
            return Err(format!(
                "referenced path is not valid UTF-8: {}",
                path.display()
            ));
        };
        check_readable(&capabilities, &path)?;
        let requested = capabilities
            .resolve(text)
            .map_err(|_| outside_workspace(&path))?;
        let unavailable = || format!("referenced path is unavailable: {}", path.display());
        let resolved = match files.inspect(FileQuery::Resolve(requested)).await {
            Ok(FileInspection::Resolved(resolved)) => resolved,
            _ => return Err(unavailable()),
        };
        let Some(kind) = resolved.kind else {
            return Err(unavailable());
        };
        if resolved.path.root().is_none() || !capabilities.access(&resolved.path).can_read() {
            return Err(outside_workspace(&path));
        }
        if !seen.insert(resolved.path.clone()) {
            continue;
        }
        let (context, file_read) = read_context(files, &capabilities, resolved.path, kind).await?;
        loaded.contexts.push(context);
        loaded.file_reads.extend(file_read);
    }
    Ok(loaded)
}

async fn read_context(
    files: &dyn FileHost,
    capabilities: &FileCapabilities,
    path: FilePath,
    kind: FileKind,
) -> Result<(InputContext, Option<(PathBuf, FileReadRecord)>), String> {
    let shown = PathBuf::from(capabilities.display(&path));
    let mentioned = capabilities
        .folder()
        .and_then(|folder| shown.strip_prefix(&folder.display_path).ok())
        .map_or_else(|| shown.clone(), Path::to_path_buf);
    match kind {
        FileKind::File if is_image_file(files, &path, &shown).await? => {
            let image = read_image(files, &path)
                .await
                .map_err(|error| format!("cannot attach image {}: {error}", mentioned.display()))?;
            let context = InputContext::Image(ImageContext {
                origin: ImageOrigin::Mentioned { path: mentioned },
                image,
            });
            Ok((context, None))
        }
        FileKind::File => {
            let file = read(files, &path, MAX_FILE_BYTES + 4)
                .await
                .map_err(|error| format!("cannot read {}: {error}", shown.display()))?;
            let (content, truncated) = text_of(&file, &shown)?;
            // The modification time comes with the content it describes.
            let file_read = file
                .metadata
                .modified
                .filter(|_| !truncated)
                .map(|modified| (shown, FileReadRecord::whole_file(&content, modified)));
            let context = InputContext::File(TextContext {
                path: mentioned,
                content,
                truncated,
            });
            Ok((context, file_read))
        }
        FileKind::Directory => {
            let (content, truncated) = read_directory(files, &path, &shown).await?;
            let context = InputContext::Directory(TextContext {
                path: mentioned,
                content,
                truncated,
            });
            Ok((context, None))
        }
        FileKind::Other => Err(format!(
            "referenced path is not a regular file or directory: {}",
            shown.display()
        )),
    }
}

/// The first `limit` bytes of the file.
async fn read(files: &dyn FileHost, path: &FilePath, limit: usize) -> Result<FileContent, String> {
    match files
        .inspect(FileQuery::Read {
            path: path.clone(),
            offset: 0,
            limit,
        })
        .await
    {
        Ok(FileInspection::Read(content)) => Ok(content),
        Ok(_) => Err("the environment answered a read with something else".into()),
        Err(error) => Err(error.message),
    }
}

async fn is_image_file(
    files: &dyn FileHost,
    path: &FilePath,
    shown: &Path,
) -> Result<bool, String> {
    let has_image_extension = shown
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        });
    if has_image_extension {
        return Ok(true);
    }
    let header = read(files, path, IMAGE_SIGNATURE_BYTES)
        .await
        .map_err(|error| format!("cannot read {}: {error}", shown.display()))?;
    Ok(image::is_supported_image(&header.data))
}

async fn read_image(files: &dyn FileHost, path: &FilePath) -> Result<Image, String> {
    let file = read(files, path, image::MAX_SOURCE_BYTES + 1).await?;
    image::prepare(&file.data).map_err(|error| error.to_string())
}

fn text_of(file: &FileContent, shown: &Path) -> Result<(String, bool), String> {
    let bytes = &file.data;
    if bytes.contains(&0) {
        return Err(format!(
            "referenced file is not UTF-8 text: {}",
            shown.display()
        ));
    }
    let length = bytes.len().min(MAX_FILE_BYTES);
    let text = match std::str::from_utf8(&bytes[..length]) {
        Ok(text) => text,
        Err(error) if error.error_len().is_none() && error.valid_up_to() > 0 => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap()
        }
        Err(_) => {
            return Err(format!(
                "referenced file is not UTF-8 text: {}",
                shown.display()
            ));
        }
    };
    Ok((text.into(), file.metadata.len > text.len() as u64))
}

/// The directory's first entries in lexical order, a directory marked with a trailing `/`.
async fn read_directory(
    files: &dyn FileHost,
    path: &FilePath,
    shown: &Path,
) -> Result<(String, bool), String> {
    let listing = match files
        .inspect(FileQuery::List {
            path: path.clone(),
            limit: MAX_DIRECTORY_ENTRIES,
        })
        .await
    {
        Ok(FileInspection::List(listing)) => listing,
        Ok(_) => {
            return Err(format!(
                "cannot list {}: the environment answered with something else",
                shown.display()
            ));
        }
        Err(error) => {
            return Err(format!(
                "cannot list {}: {}",
                shown.display(),
                error.message
            ));
        }
    };
    let mut entries = Vec::with_capacity(listing.entries.len());
    for entry in listing.entries {
        if entry.name.chars().any(char::is_control) {
            return Err(format!(
                "directory contains a control character in a name: {}",
                shown.display()
            ));
        }
        let suffix = if entry.kind == FileKind::Directory {
            "/"
        } else {
            ""
        };
        entries.push(format!("{}{suffix}", entry.name));
    }
    entries.sort();
    Ok((entries.join("\n"), listing.truncated))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tickloop_harness_tools::local_roots::LocalRoots;

    use super::*;

    fn folder(directory: &Path) -> LocalRoots {
        LocalRoots::new(directory.to_str(), &[])
    }

    #[test]
    fn resource_contexts_are_bounded_but_repeated_uris_keep_block_order() {
        let resource = |uri: &str, content: Option<String>| {
            InputContext::Resource(tickloop_harness_protocol::ResourceContext {
                uri: uri.into(),
                mime_type: Some("text/plain".into()),
                name: None,
                content,
            })
        };
        let uri = "file:///not-read.txt";
        assert!(restored_contexts_are_valid(&[
            resource(uri, Some("first".into())),
            resource(uri, Some("second".into())),
            resource("https://never-fetched.invalid/resource", None),
        ]));
        assert!(!restored_contexts_are_valid(&[resource("", None)]));
        assert!(!restored_contexts_are_valid(&[resource(
            &"x".repeat(MAX_URI_BYTES + 1),
            None
        )]));
        assert!(!restored_contexts_are_valid(&[
            resource(uri, Some("x".repeat(MAX_RESOURCE_BYTES))),
            resource(uri, Some("x".into())),
        ]));
        assert!(!restored_contexts_are_valid(&vec![
            resource(uri, None);
            MAX_CONTEXTS + 1
        ]));
    }

    #[tokio::test]
    async fn loads_files_and_immediate_directory_entries_without_following_outside_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("notes.txt"), "hello").unwrap();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/main.rs"), "fn main() {}").unwrap();

        let loaded = load(
            &folder(directory.path()),
            vec![
                directory.path().join("notes.txt"),
                directory.path().join("src"),
            ],
        )
        .await
        .unwrap();
        let contexts = loaded.contexts;
        let [(read_path, record)] = loaded.file_reads.as_slice() else {
            panic!("only the attached file counts as read");
        };
        assert_eq!(
            read_path,
            &directory.path().canonicalize().unwrap().join("notes.txt")
        );
        assert_eq!(record.content, "hello");
        assert!(record.is_full_read);

        assert_eq!(
            contexts,
            vec![
                InputContext::File(TextContext {
                    path: PathBuf::from("notes.txt"),
                    content: "hello".into(),
                    truncated: false,
                }),
                InputContext::Directory(TextContext {
                    path: PathBuf::from("src"),
                    content: "main.rs".into(),
                    truncated: false,
                }),
            ]
        );
    }

    #[test]
    fn file_and_directory_contexts_keep_their_stored_json_shape() {
        let stored = serde_json::json!([
            {"path":"notes.txt","kind":"file","content":"hello","truncated":false},
            {"path":"src","kind":"directory","content":"main.rs","truncated":true},
        ]);

        let contexts: Vec<InputContext> = serde_json::from_value(stored.clone()).unwrap();

        assert_eq!(serde_json::to_value(&contexts).unwrap(), stored);
        assert_eq!(contexts[1].path(), Some(Path::new("src")));
    }

    #[tokio::test]
    async fn rejects_paths_outside_the_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();

        let error = load(&folder(directory.path()), vec![outside.path().into()])
            .await
            .err()
            .unwrap();

        assert!(error.contains("outside the configured workspace"));
    }

    #[tokio::test]
    async fn directory_listing_keeps_the_same_lexicographic_prefix_regardless_of_creation_order() {
        let directory = tempfile::tempdir().unwrap();
        for index in (0..=MAX_DIRECTORY_ENTRIES).rev() {
            fs::write(directory.path().join(format!("entry-{index:04}")), "").unwrap();
        }
        let files = folder(directory.path());
        let root = files.capabilities().folder().unwrap().name.clone();

        let (content, truncated) = read_directory(
            &files,
            &FilePath::in_root(root, "").unwrap(),
            directory.path(),
        )
        .await
        .unwrap();

        assert!(truncated);
        assert_eq!(content.lines().count(), MAX_DIRECTORY_ENTRIES);
        assert_eq!(content.lines().next(), Some("entry-0000"));
        assert_eq!(content.lines().last(), Some("entry-0999"));
    }
}

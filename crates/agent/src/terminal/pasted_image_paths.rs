//! Image file paths inside bracketed-paste text, as dragging a file into the terminal or
//! copying its path produces them (Claude Code `usePasteHandler` and `imagePaste.ts`,
//! Codex `normalize_pasted_path`).

use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};

use reqwest::Url;
use tickloop_agent::image::{self, Image, ImageError};

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];
const FILE_URL_SCHEME: &str = "file";
/// Several files dragged at once arrive on one line separated by spaces; a space followed by
/// one of these starts the next path. Spaces inside a path are shell-escaped or quoted.
const PATH_STARTS: &[&str] = &["/", "~/", "'", "\"", "file://"];

/// A span of the pasted text that names a file with an image extension.
#[derive(Debug)]
pub(super) struct PastedImagePath {
    pub(super) range: Range<usize>,
    pub(super) path: PathBuf,
}

/// Finds the pasted lines, or space-separated paths within a line, that name image files, in
/// paste order. Surrounding quotes, shell backslash escapes and `file://` URLs are undone and
/// `~/` expands to the home directory; a relative path is taken from `working_directory`, the
/// Chat's working directory. Whether the file exists is left to the caller.
pub(super) fn find_image_paths(pasted: &str, working_directory: &Path) -> Vec<PastedImagePath> {
    let mut found = Vec::new();
    let mut line_start = 0;
    for line in pasted.split_inclusive('\n') {
        let mut candidate_start = 0;
        for split in path_split_points(line).chain([line.len()]) {
            let candidate = &line[candidate_start..split];
            let trimmed = candidate.trim();
            if let Some(path) = image_path(trimmed, working_directory) {
                let start =
                    line_start + candidate_start + (candidate.len() - candidate.trim_start().len());
                found.push(PastedImagePath {
                    range: start..start + trimmed.len(),
                    path,
                });
            }
            candidate_start = split;
        }
        line_start += line.len();
    }
    found
}

fn path_split_points(line: &str) -> impl Iterator<Item = usize> + '_ {
    line.char_indices()
        .filter(move |&(index, character)| {
            character == ' '
                && !line[..index].ends_with('\\')
                && PATH_STARTS
                    .iter()
                    .any(|start| line[index + 1..].starts_with(start))
        })
        .map(|(index, _)| index)
}

fn image_path(candidate: &str, working_directory: &Path) -> Option<PathBuf> {
    let unquoted = strip_outer_quotes(candidate);
    let is_quoted = unquoted.len() != candidate.len();
    let path = match Url::parse(unquoted) {
        Ok(url) if url.scheme() == FILE_URL_SCHEME => url.to_file_path().ok()?,
        _ if is_quoted => PathBuf::from(unquoted),
        _ => PathBuf::from(strip_shell_escapes(unquoted)),
    };
    let has_image_extension = path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|image| extension.eq_ignore_ascii_case(image))
        });
    if !has_image_extension {
        return None;
    }
    if let Ok(relative_to_home) = path.strip_prefix("~") {
        return std::env::var_os("HOME").map(|home| PathBuf::from(home).join(relative_to_home));
    }
    Some(working_directory.join(path))
}

fn strip_outer_quotes(text: &str) -> &str {
    ['"', '\'']
        .into_iter()
        .find_map(|quote| {
            text.strip_prefix(quote)
                .and_then(|inner| inner.strip_suffix(quote))
        })
        .unwrap_or(text)
}

/// `name\ \(1\).png` becomes `name (1).png` and `\\` a single backslash, as a shell would
/// read an unquoted word.
fn strip_shell_escapes(text: &str) -> String {
    let mut unescaped = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        let escaped = (character == '\\').then(|| characters.next()).flatten();
        unescaped.push(escaped.unwrap_or(character));
    }
    unescaped
}

/// `None` when the file cannot be read at all, so its pasted path simply stays text.
pub(super) fn read_pasted_image(path: &Path) -> Option<Result<Image, ImageError>> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| {
            file.take(image::MAX_SOURCE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
        })
        .ok()?;
    Some(image::prepare(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found<'a>(pasted: &'a str, working_directory: &Path) -> Vec<(&'a str, PathBuf)> {
        find_image_paths(pasted, working_directory)
            .into_iter()
            .map(|candidate| (&pasted[candidate.range], candidate.path))
            .collect()
    }

    #[test]
    fn files_dragged_together_split_at_spaces_that_start_a_new_path() {
        let pasted = "/shots/a\\ b.png '/shots/c d.jpeg' file:///shots/e%20f.gif ~/g.webp";
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());

        assert_eq!(
            found(pasted, Path::new("/project")),
            vec![
                ("/shots/a\\ b.png", PathBuf::from("/shots/a b.png")),
                ("'/shots/c d.jpeg'", PathBuf::from("/shots/c d.jpeg")),
                ("file:///shots/e%20f.gif", PathBuf::from("/shots/e f.gif")),
                ("~/g.webp", home.join("g.webp")),
            ]
        );
    }

    #[test]
    fn only_whole_lines_or_path_segments_with_image_extensions_are_candidates() {
        let pasted = "see /shots/a.png for details\n  charts/b.JPG  \nnotes.txt\n/shots/c.bmp";

        assert_eq!(
            found(pasted, Path::new("/project")),
            vec![("charts/b.JPG", PathBuf::from("/project/charts/b.JPG"))]
        );
    }
}

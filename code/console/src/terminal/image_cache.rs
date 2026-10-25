//! Pasted images kept on disk so `[Image #N]` can link to them, after Claude Code
//! `utils/imageStore.ts` and `components/ClickableImageRef.tsx`.
//!
//! Files live at `<root>/image-cache/<chatId>/<N>.<ext>`, private to the user. Claude Code
//! keys the directory by process; Tickloop keys it by Chat because one process hosts many
//! Chats and several processes may run at once. The cache only serves links: nothing reads it
//! back as model context.

use std::{
    fs, io,
    ops::Range,
    path::{Path, PathBuf},
};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use uuid::Uuid;

use super::hyperlink::Hyperlink;
use tickloop_code_settings::IMAGE_CACHE_DIRECTORY;
use tickloop_harness_base::private_fs::{
    PRIVATE_FILE_MODE, create_private_directory, replace_file_atomically,
};
use tickloop_harness_protocol::{Image, ImageMediaType};

const PLACEHOLDER_PREFIX: &str = "[Image #";
const PLACEHOLDER_SUFFIX: char = ']';
const CACHED_MEDIA_TYPES: [ImageMediaType; 3] = [
    ImageMediaType::Png,
    ImageMediaType::Jpeg,
    ImageMediaType::Webp,
];

#[derive(Clone, Debug)]
pub(super) struct ImageCache {
    directory: PathBuf,
}

impl ImageCache {
    pub(super) fn in_root(root: &Path) -> Self {
        Self {
            directory: root.join(IMAGE_CACHE_DIRECTORY),
        }
    }

    /// Writes `image` as `[Image #id]` of `chat`, replacing any earlier file of that number.
    pub(super) fn store(&self, chat: Uuid, id: u32, image: &Image) -> io::Result<PathBuf> {
        let bytes = BASE64
            .decode(&image.data)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let chat_directory = self.chat_directory(chat);
        create_private_directory(&chat_directory)?;
        let path = chat_directory.join(file_name(id, image.media_type));
        replace_file_atomically(&path, &bytes, PRIVATE_FILE_MODE)?;
        Ok(path)
    }

    /// The cached file of `[Image #id]` in `chat`, when it is still a regular file.
    pub(super) fn find(&self, chat: Uuid, id: u32) -> Option<PathBuf> {
        let chat_directory = self.chat_directory(chat);
        CACHED_MEDIA_TYPES
            .into_iter()
            .map(|media_type| chat_directory.join(file_name(id, media_type)))
            .find(|path| fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()))
    }

    fn chat_directory(&self, chat: Uuid) -> PathBuf {
        self.directory.join(chat.hyphenated().to_string())
    }
}

/// Claude Code names the file after the media subtype: `image/jpeg` is `<N>.jpeg`.
fn file_name(id: u32, media_type: ImageMediaType) -> String {
    let extension = media_type
        .mime_type()
        .split_once('/')
        .map_or("png", |(_, subtype)| subtype);
    format!("{id}.{extension}")
}

/// Resolves `[Image #N]` in user messages to links. Links exist only when the terminal
/// shows them and the image is still in the cache; everything else stays plain text.
#[derive(Clone, Debug, Default)]
pub(super) struct ImageLinks {
    cache: Option<ImageCache>,
}

impl ImageLinks {
    pub(super) fn new(cache: Option<ImageCache>, terminal_shows_links: bool) -> Self {
        Self {
            cache: cache.filter(|_| terminal_shows_links),
        }
    }

    /// The byte ranges of `text` holding a cached `[Image #N]` of `chat`, in order.
    pub(super) fn in_text(&self, chat: Uuid, text: &str) -> Vec<(Range<usize>, Hyperlink)> {
        let Some(cache) = &self.cache else {
            return Vec::new();
        };
        placeholders(text)
            .filter_map(|(range, id)| {
                let link = Hyperlink::file(&cache.find(chat, id)?)?;
                Some((range, link))
            })
            .collect()
    }
}

fn placeholders(text: &str) -> impl Iterator<Item = (Range<usize>, u32)> + '_ {
    text.match_indices(PLACEHOLDER_PREFIX)
        .filter_map(move |(start, _)| {
            let digits_start = start + PLACEHOLDER_PREFIX.len();
            let rest = &text[digits_start..];
            let digits_length = rest
                .find(|character: char| !character.is_ascii_digit())
                .unwrap_or(rest.len());
            if !rest[digits_length..].starts_with(PLACEHOLDER_SUFFIX) {
                return None;
            }
            let id = rest[..digits_length].parse().ok()?;
            Some((start..digits_start + digits_length + 1, id))
        })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn png(width: u32, height: u32) -> Image {
        let mut bytes = Vec::new();
        ::image::DynamicImage::ImageRgba8(::image::RgbaImage::new(width, height))
            .write_to(&mut io::Cursor::new(&mut bytes), ::image::ImageFormat::Png)
            .unwrap();
        tickloop_harness_base::image::prepare(&bytes).unwrap()
    }

    #[test]
    fn stored_images_are_private_files_named_by_chat_and_number() {
        let root = tempfile::tempdir().unwrap();
        let cache = ImageCache::in_root(root.path());
        let chat = Uuid::new_v4();
        let image = png(4, 3);

        let path = cache.store(chat, 2, &image).unwrap();

        assert_eq!(
            path,
            root.path()
                .join("image-cache")
                .join(chat.to_string())
                .join("2.png")
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            BASE64.decode(&image.data).unwrap()
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(cache.find(chat, 2), Some(path));
        assert_eq!(cache.find(chat, 1), None);
        assert_eq!(cache.find(Uuid::new_v4(), 2), None);
    }

    #[test]
    fn only_cached_placeholders_of_the_chat_become_links_in_a_terminal_that_shows_them() {
        let root = tempfile::tempdir().unwrap();
        let cache = ImageCache::in_root(root.path());
        let chat = Uuid::new_v4();
        let cached = cache.store(chat, 1, &png(2, 2)).unwrap();
        let text = "see [Image #1], [Image #2], [Image #x] and [Image #1";

        let links = ImageLinks::new(Some(cache.clone()), true).in_text(chat, text);

        assert_eq!(
            links,
            [(4..14, Hyperlink::file(&cached).unwrap())],
            "an uncached or malformed placeholder stays plain"
        );
        assert!(
            ImageLinks::new(Some(cache.clone()), false)
                .in_text(chat, text)
                .is_empty()
        );
        assert!(
            ImageLinks::new(Some(cache), true)
                .in_text(Uuid::new_v4(), text)
                .is_empty()
        );
        assert!(ImageLinks::default().in_text(chat, text).is_empty());
    }
}

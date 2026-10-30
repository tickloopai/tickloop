use std::{io::Cursor, sync::Mutex};

use ::image::{
    DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, codecs::jpeg::JpegEncoder,
    imageops::FilterType,
};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use tickloop_harness_protocol::{Image, ImageMediaType};

/// Largest source file accepted for preparation; callers read at most this many bytes plus one.
pub const MAX_SOURCE_BYTES: usize = 32 * 1024 * 1024;
/// Anthropic's per-image base64 limit, shared by every image entry point.
pub const MAX_BASE64_BYTES: usize = 5 * 1024 * 1024;
/// Source pixel and decoder allocation budgets, checked before allocating decoded pixels.
pub const MAX_DECODED_PIXELS: u64 = 32 * 1024 * 1024;
pub const MAX_DECODED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SOURCE_DIMENSION: u32 = 32 * 1024;
// Preparation can hold source, decoded and resized buffers at once. Serialize all entry points
// so independent ACP requests and clipboard workers cannot multiply that working set.
static PREPARATION: Mutex<()> = Mutex::new(());
const MAX_DIMENSION: u32 = 2000;
const JPEG_FALLBACK_QUALITY: u8 = 80;

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum ImageError {
    #[error("image file is empty")]
    Empty,
    #[error("unsupported image format; use PNG, JPEG, GIF or WebP")]
    UnsupportedFormat,
    #[error("image could not be decoded: {0}")]
    Undecodable(String),
    #[error("image could not be re-encoded: {0}")]
    Encode(String),
    #[error("image is too large to send even after compression")]
    TooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

/// Whether the bytes start with the signature of an image format that `prepare` accepts.
pub fn is_supported_image(bytes: &[u8]) -> bool {
    source_format(bytes).is_some()
}

/// The MIME type of the image format whose signature starts the bytes: PNG, JPEG, GIF or WebP.
pub fn signature_media_type(bytes: &[u8]) -> Option<&'static str> {
    source_format(bytes).map(SourceFormat::media_type)
}

/// Turns image file bytes into a model-ready image: longest side at most 2000px in the
/// original format, GIF as PNG, and one JPEG re-encode when the base64 exceeds 5 MB.
pub fn prepare(bytes: &[u8]) -> Result<Image, ImageError> {
    if bytes.is_empty() {
        return Err(ImageError::Empty);
    }
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(ImageError::TooLarge);
    }
    let _guard = PREPARATION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let format = source_format(bytes).ok_or(ImageError::UnsupportedFormat)?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format.decoder_format());
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(decode_error)?;
    let (width, height) = decoder.dimensions();
    check_dimensions(width, height)?;
    if decoder.total_bytes() > MAX_DECODED_BYTES {
        return Err(ImageError::TooLarge);
    }
    let decoded = DynamicImage::from_decoder(decoder).map_err(decode_error)?;
    let (pixels, encoded, media_type) = if width.max(height) > MAX_DIMENSION {
        let resized = decoded.resize(MAX_DIMENSION, MAX_DIMENSION, FilterType::Triangle);
        let media_type = format.output_media_type();
        let encoded = encode(&resized, media_type)?;
        (resized, encoded, media_type)
    } else if format == SourceFormat::Gif {
        let encoded = encode(&decoded, ImageMediaType::Png)?;
        (decoded, encoded, ImageMediaType::Png)
    } else {
        (decoded, bytes.to_vec(), format.output_media_type())
    };
    finish(pixels, encoded, media_type)
}

/// Prepares owned clipboard RGBA pixels without encoding and decoding the full-size image.
/// Dimensions must be nonzero and the buffer must contain exactly four bytes per pixel.
/// Unlike file input, raw pixels are not subject to the encoded source-file size limit.
/// PNG preserves transparency unless the shared 5 MB base64 limit requires JPEG fallback.
pub fn prepare_rgba(width: u32, height: u32, bytes: Vec<u8>) -> Result<Image, ImageError> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4));
    if width == 0 || height == 0 || expected != Some(bytes.len()) {
        return Err(ImageError::Undecodable(
            "invalid RGBA dimensions or buffer length".into(),
        ));
    }
    check_dimensions(width, height)?;
    if bytes.len() as u64 > MAX_DECODED_BYTES {
        return Err(ImageError::TooLarge);
    }
    let _guard = PREPARATION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rgba = ::image::RgbaImage::from_raw(width, height, bytes)
        .ok_or_else(|| ImageError::Undecodable("invalid RGBA image".into()))?;
    let decoded = DynamicImage::ImageRgba8(rgba);
    let pixels = if width.max(height) > MAX_DIMENSION {
        decoded.resize(MAX_DIMENSION, MAX_DIMENSION, FilterType::Triangle)
    } else {
        decoded
    };
    let encoded = encode(&pixels, ImageMediaType::Png)?;
    finish(pixels, encoded, ImageMediaType::Png)
}

fn check_dimensions(width: u32, height: u32) -> Result<(), ImageError> {
    if width > MAX_SOURCE_DIMENSION
        || height > MAX_SOURCE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_DECODED_PIXELS
    {
        return Err(ImageError::TooLarge);
    }
    Ok(())
}

fn decode_error(error: ::image::ImageError) -> ImageError {
    match error {
        ::image::ImageError::Limits(_) => ImageError::TooLarge,
        error => ImageError::Undecodable(error.to_string()),
    }
}

fn finish(
    pixels: DynamicImage,
    encoded: Vec<u8>,
    media_type: ImageMediaType,
) -> Result<Image, ImageError> {
    let mut image = Image {
        media_type,
        data: BASE64.encode(&encoded),
        width: pixels.width(),
        height: pixels.height(),
    };
    if image.data.len() > MAX_BASE64_BYTES {
        image.media_type = ImageMediaType::Jpeg;
        image.data = BASE64.encode(encode(&pixels, ImageMediaType::Jpeg)?);
        if image.data.len() > MAX_BASE64_BYTES {
            return Err(ImageError::TooLarge);
        }
    }
    Ok(image)
}

fn source_format(bytes: &[u8]) -> Option<SourceFormat> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some(SourceFormat::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(SourceFormat::Jpeg)
    } else if bytes.starts_with(b"GIF8") {
        Some(SourceFormat::Gif)
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(SourceFormat::Webp)
    } else {
        None
    }
}

impl SourceFormat {
    fn decoder_format(self) -> ImageFormat {
        match self {
            Self::Png => ImageFormat::Png,
            Self::Jpeg => ImageFormat::Jpeg,
            Self::Gif => ImageFormat::Gif,
            Self::Webp => ImageFormat::WebP,
        }
    }

    fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    fn output_media_type(self) -> ImageMediaType {
        match self {
            Self::Png | Self::Gif => ImageMediaType::Png,
            Self::Jpeg => ImageMediaType::Jpeg,
            Self::Webp => ImageMediaType::Webp,
        }
    }
}

fn encode(image: &DynamicImage, media_type: ImageMediaType) -> Result<Vec<u8>, ImageError> {
    let mut bytes = Vec::new();
    let result = match media_type {
        ImageMediaType::Png => image.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png),
        ImageMediaType::Webp => image.write_to(&mut Cursor::new(&mut bytes), ImageFormat::WebP),
        ImageMediaType::Jpeg => JpegEncoder::new_with_quality(&mut bytes, JPEG_FALLBACK_QUALITY)
            .encode_image(&DynamicImage::ImageRgb8(image.to_rgb8())),
    };
    result.map_err(|error| ImageError::Encode(error.to_string()))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::image::{GenericImageView, Rgb, RgbImage, Rgba, RgbaImage};

    fn gradient(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_fn(width, height, |x, y| {
            Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        }))
    }

    fn encoded(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut bytes = Vec::new();
        let image = if format == ImageFormat::Jpeg {
            DynamicImage::ImageRgb8(image.to_rgb8())
        } else {
            image.clone()
        };
        image
            .write_to(&mut Cursor::new(&mut bytes), format)
            .unwrap();
        bytes
    }

    fn decoded(image: &Image) -> DynamicImage {
        ::image::load_from_memory(&BASE64.decode(&image.data).unwrap()).unwrap()
    }

    #[test]
    fn small_rgba_images_keep_dimensions_and_transparent_pixels() {
        let rgba = RgbaImage::from_fn(4, 3, |x, y| Rgba([x as u8, y as u8, 128, (x * 60) as u8]));

        let image = prepare_rgba(4, 3, rgba.clone().into_raw()).unwrap();

        assert_eq!(image.media_type, ImageMediaType::Png);
        assert_eq!((image.width, image.height), (4, 3));
        assert_eq!(decoded(&image).to_rgba8(), rgba);
        assert!(image.data.len() <= MAX_BASE64_BYTES);
    }

    #[test]
    fn large_rgba_images_resize_before_encoding_without_a_source_file_limit() {
        let rgba = RgbaImage::from_pixel(4200, 2100, Rgba([80, 120, 160, 128]));
        assert!(rgba.as_raw().len() > MAX_SOURCE_BYTES);

        let image = prepare_rgba(4200, 2100, rgba.into_raw()).unwrap();

        assert_eq!(image.media_type, ImageMediaType::Png);
        assert_eq!((image.width, image.height), (2000, 1000));
        let pixels = decoded(&image).to_rgba8();
        assert_eq!(pixels.dimensions(), (2000, 1000));
        assert_eq!(pixels.get_pixel(1000, 500)[3], 128);
        assert!(image.data.len() <= MAX_BASE64_BYTES);
    }

    #[test]
    fn rgba_images_at_the_dimension_limit_are_not_resized() {
        let image = prepare_rgba(2000, 1, vec![255; 2000 * 4]).unwrap();

        assert_eq!((image.width, image.height), (2000, 1));
        assert_eq!(decoded(&image).dimensions(), (2000, 1));
    }

    #[test]
    fn invalid_rgba_dimensions_and_buffer_lengths_are_rejected() {
        for (width, height, bytes) in [
            (0, 1, vec![]),
            (1, 0, vec![]),
            (0, 0, vec![]),
            (1, 1, vec![0; 3]),
            (1, 1, vec![0; 5]),
            (u32::MAX, u32::MAX, vec![]),
        ] {
            assert!(matches!(
                prepare_rgba(width, height, bytes),
                Err(ImageError::Undecodable(_))
            ));
        }
    }

    #[test]
    fn oversized_source_files_are_rejected_before_decoding() {
        assert_eq!(
            prepare(&vec![0; MAX_SOURCE_BYTES + 1]),
            Err(ImageError::TooLarge)
        );
    }

    #[test]
    fn compressed_images_cannot_allocate_an_unbounded_pixel_buffer() {
        for (width, height) in [(8192u32, 8192u32), (MAX_SOURCE_DIMENSION + 1, 1)] {
            let mut bytes = encoded(&gradient(1, 1), ImageFormat::Png);
            bytes[16..20].copy_from_slice(&width.to_be_bytes());
            bytes[20..24].copy_from_slice(&height.to_be_bytes());
            // Recompute the IHDR CRC so rejection is due to the budget, not corrupt input.
            let mut crc = !0u32;
            for byte in &bytes[12..29] {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xedb8_8320u32 & (0u32.wrapping_sub(crc & 1)));
                }
            }
            bytes[29..33].copy_from_slice(&(!crc).to_be_bytes());
            assert_eq!(prepare(&bytes), Err(ImageError::TooLarge));
        }
    }

    #[test]
    fn decoded_pixel_and_memory_budgets_are_bounded() {
        assert_eq!(check_dimensions(8192, 8192), Err(ImageError::TooLarge));
        assert_eq!(check_dimensions(4200, 2100), Ok(()));
        assert_eq!(MAX_DECODED_PIXELS * 4, MAX_DECODED_BYTES);
    }

    #[test]
    fn small_images_keep_their_bytes_and_format_detected_from_the_header() {
        for (format, media_type) in [
            (ImageFormat::Png, ImageMediaType::Png),
            (ImageFormat::Jpeg, ImageMediaType::Jpeg),
            (ImageFormat::WebP, ImageMediaType::Webp),
        ] {
            let bytes = encoded(&gradient(40, 30), format);

            let image = prepare(&bytes).unwrap();

            assert_eq!(image.media_type, media_type);
            assert_eq!(BASE64.decode(&image.data).unwrap(), bytes);
            assert_eq!((image.width, image.height), (40, 30));
            assert!(is_supported_image(&bytes));
        }
    }

    #[test]
    fn images_longer_than_the_limit_shrink_proportionally_in_their_original_format() {
        for (format, media_type) in [
            (ImageFormat::Png, ImageMediaType::Png),
            (ImageFormat::Jpeg, ImageMediaType::Jpeg),
        ] {
            let bytes = encoded(&gradient(4000, 1000), format);

            let image = prepare(&bytes).unwrap();

            assert_eq!(image.media_type, media_type);
            assert_eq!((image.width, image.height), (2000, 500));
            assert_eq!(decoded(&image).dimensions(), (2000, 500));
        }
    }

    #[test]
    fn images_at_the_limit_are_not_resized() {
        let bytes = encoded(&gradient(2000, 20), ImageFormat::Png);

        let image = prepare(&bytes).unwrap();

        assert_eq!((image.width, image.height), (2000, 20));
        assert_eq!(BASE64.decode(&image.data).unwrap(), bytes);
    }

    #[test]
    fn gif_images_become_png() {
        let bytes = encoded(&gradient(12, 8), ImageFormat::Gif);

        let image = prepare(&bytes).unwrap();

        assert_eq!(image.media_type, ImageMediaType::Png);
        assert_eq!((image.width, image.height), (12, 8));
        assert_eq!(
            ::image::guess_format(&BASE64.decode(&image.data).unwrap()).unwrap(),
            ImageFormat::Png
        );
    }

    #[test]
    fn images_whose_base64_exceeds_the_limit_are_reencoded_as_jpeg_once() {
        // Low-amplitude noise defeats PNG compression but JPEG smooths it away.
        let mut seed = 0x2545_f491_u32;
        let noisy = RgbImage::from_fn(1800, 1800, |x, y| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let noise = (seed % 24) as u8;
            Rgb([
                (x / 8) as u8 ^ noise,
                (y / 8) as u8 ^ noise,
                noise.wrapping_mul(7),
            ])
        });
        let noisy = DynamicImage::ImageRgb8(noisy);
        let bytes = encoded(&noisy, ImageFormat::Png);
        assert!(BASE64.encode(&bytes).len() > MAX_BASE64_BYTES);

        let rgba_image = prepare_rgba(1800, 1800, noisy.to_rgba8().into_raw()).unwrap();
        assert_eq!(rgba_image.media_type, ImageMediaType::Jpeg);
        assert!(rgba_image.data.len() <= MAX_BASE64_BYTES);
        assert_eq!((rgba_image.width, rgba_image.height), (1800, 1800));
        assert_eq!(decoded(&rgba_image).dimensions(), (1800, 1800));

        let image = prepare(&bytes).unwrap();

        assert_eq!(image.media_type, ImageMediaType::Jpeg);
        assert!(image.data.len() <= MAX_BASE64_BYTES);
        assert_eq!((image.width, image.height), (1800, 1800));
        assert_eq!(
            ::image::guess_format(&BASE64.decode(&image.data).unwrap()).unwrap(),
            ImageFormat::Jpeg
        );
    }

    #[test]
    fn empty_unsupported_and_corrupt_inputs_report_distinct_errors() {
        let mut corrupt_png = encoded(&gradient(4, 4), ImageFormat::Png);
        corrupt_png.truncate(20);

        assert_eq!(prepare(&[]), Err(ImageError::Empty));
        assert_eq!(prepare(b"BM\x00\x00"), Err(ImageError::UnsupportedFormat));
        assert_eq!(prepare(b"plain text"), Err(ImageError::UnsupportedFormat));
        assert!(matches!(
            prepare(&corrupt_png),
            Err(ImageError::Undecodable(_))
        ));
        assert!(!is_supported_image(b"BM\x00\x00"));
    }

    #[test]
    fn signatures_name_the_mime_type_of_the_original_format() {
        for (format, media_type) in [
            (ImageFormat::Png, "image/png"),
            (ImageFormat::Jpeg, "image/jpeg"),
            (ImageFormat::Gif, "image/gif"),
            (ImageFormat::WebP, "image/webp"),
        ] {
            let bytes = encoded(&gradient(4, 4), format);
            assert_eq!(signature_media_type(&bytes), Some(media_type), "{format:?}");
        }
        assert_eq!(signature_media_type(b"<svg></svg>"), None);
    }
}

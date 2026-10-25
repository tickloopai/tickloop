//! Artifact images: files a Slides artifact holds and its Slides show as `asset:<asset_id>`.

use std::time::Duration;

use reqwest::{Method, header};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{
    ArtifactClient, ArtifactError, ArtifactOperation, CredentialSnapshot, MAX_RESPONSE_BYTES,
    bearer, path_segment, service_error,
};

/// The only Artifact format that holds images.
pub const IMAGE_ARTIFACT_FORMAT: &str = "slides";
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_SVG_BYTES: usize = 2 * 1024 * 1024;
const SVG_MEDIA_TYPE: &str = "image/svg+xml";
/// One upload carries up to 5 MiB, longer than an ordinary service call may take.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_ASSET_ID_BYTES: usize = 256;
const SHA256_HEX_DIGITS: usize = 64;

/// An image the service holds for one artifact, as its upload or operation receipt reports it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ArtifactImage {
    pub asset_id: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub content_type: String,
    pub bytes: u64,
    pub sha256: String,
    pub width: Option<u64>,
    pub height: Option<u64>,
}

/// What a receipt must report for the image this client sent. The service strips metadata such as
/// EXIF from the image it stores, so the receipt's size and digest describe the stored file rather
/// than the one sent; they only have to be well formed.
struct SentImage<'a> {
    content_type: &'a str,
}

impl SentImage<'_> {
    fn is_reported_by(&self, image: &ArtifactImage) -> bool {
        !image.asset_id.is_empty()
            && image.asset_id.len() <= MAX_ASSET_ID_BYTES
            && !image.asset_id.chars().any(char::is_control)
            && image
                .reference
                .strip_prefix("asset:")
                .is_some_and(|id| super::artifact_ids_match(&image.asset_id, id))
            && image.content_type == self.content_type
            && (1..=MAX_IMAGE_BYTES as u64).contains(&image.bytes)
            && image.sha256.len() == SHA256_HEX_DIGITS
            && image
                .sha256
                .bytes()
                .all(|digit| matches!(digit, b'0'..=b'9' | b'a'..=b'f'))
    }
}

/// The content type of an image a Slides artifact accepts, told by the file's leading bytes, once
/// its size is within that type's limit.
pub fn artifact_image_type(bytes: &[u8]) -> Result<&'static str, String> {
    let content_type = tickloop_harness_base::image::signature_media_type(bytes)
        .or_else(|| is_svg(bytes).then_some(SVG_MEDIA_TYPE))
        .ok_or("the file is not a PNG, JPEG, GIF, WebP or SVG image")?;
    let limit = if content_type == SVG_MEDIA_TYPE {
        MAX_SVG_BYTES
    } else {
        MAX_IMAGE_BYTES
    };
    if bytes.len() > limit {
        return Err(format!(
            "the {content_type} file has {} bytes; an Artifact image holds at most 5 MiB, an SVG at most 2 MiB",
            bytes.len()
        ));
    }
    Ok(content_type)
}

/// UTF-8 XML whose root element is `svg`, after any declaration, comments and doctype.
fn is_svg(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let mut rest = text.strip_prefix('\u{feff}').unwrap_or(text);
    loop {
        rest = rest.trim_start();
        let (prologue_end, after_opening) = if let Some(after) = rest.strip_prefix("<?") {
            ("?>", after)
        } else if let Some(after) = rest.strip_prefix("<!--") {
            ("-->", after)
        } else if let Some(after) = rest.strip_prefix("<!") {
            (">", after)
        } else {
            break;
        };
        let Some(end) = after_opening.find(prologue_end) else {
            return false;
        };
        rest = &after_opening[end + prologue_end.len()..];
    }
    rest.strip_prefix("<svg").is_some_and(|after| {
        after.starts_with(|character: char| {
            character.is_ascii_whitespace() || matches!(character, '>' | '/')
        })
    })
}

impl ArtifactClient {
    /// Uploads one image to a Slides artifact as `operation_id`. When the response is lost or
    /// cannot be read, it asks the service for that operation's receipt instead of sending the
    /// image again; without a committed receipt the outcome stays unknown.
    pub async fn upload_image(
        &self,
        artifact_id: &str,
        content_type: &str,
        bytes: Vec<u8>,
        operation_id: Uuid,
        credential: &CredentialSnapshot,
    ) -> Result<ArtifactImage, ArtifactError> {
        if !self
            .credential_snapshot()?
            .is_some_and(|current| self.credentials_match(credential, &current))
        {
            return Err(ArtifactError::AccountChanged);
        }
        let artifact_path = format!("/api/v1/artifacts/{}", path_segment(artifact_id)?);
        let sent = SentImage { content_type };
        let request = self
            .request(Method::POST, &format!("{artifact_path}/assets"))?
            .query(&[("operation_id", operation_id.to_string())])
            .header(header::AUTHORIZATION, bearer(&credential.access_token)?)
            .header(header::CONTENT_TYPE, content_type)
            .timeout(UPLOAD_TIMEOUT)
            .body(bytes);
        let response = self.send_authenticated(request, credential).await;
        if matches!(response, Err(ArtifactError::AccessDenied)) {
            return Err(ArtifactError::AccessDenied);
        }
        if let Ok(response) = response {
            let status = response.status();
            if status.is_success() {
                if let Ok(image) = self
                    .read_json_with_limit::<ArtifactImage>(response, MAX_RESPONSE_BYTES)
                    .await
                    && sent.is_reported_by(&image)
                {
                    return Ok(image);
                }
            } else if !status.is_server_error() {
                let (code, message, reason) = service_error(
                    &self
                        .read_json_with_limit::<Value>(response, MAX_RESPONSE_BYTES)
                        .await
                        .unwrap_or_default(),
                );
                return Err(ArtifactError::Service {
                    status: status.as_u16(),
                    code,
                    message,
                    reason,
                });
            }
        }
        self.uploaded_image(&artifact_path, operation_id, &sent, credential)
            .await
    }

    async fn uploaded_image(
        &self,
        artifact_path: &str,
        operation_id: Uuid,
        sent: &SentImage<'_>,
        credential: &CredentialSnapshot,
    ) -> Result<ArtifactImage, ArtifactError> {
        let unknown = |outcome: String| ArtifactError::OutcomeUnknown {
            operation_id: operation_id.to_string(),
            outcome,
        };
        let receipt: ArtifactOperation = self
            .catalog_request_with_credential(
                Method::GET,
                &format!("{artifact_path}/operations/{operation_id}"),
                &[],
                None,
                MAX_RESPONSE_BYTES,
                credential,
            )
            .await
            .map_err(|error| unknown(error.to_string()))?;
        if receipt.operation_id != operation_id.to_string() || receipt.status != "committed" {
            return Err(unknown(receipt.status));
        }
        receipt
            .result
            .and_then(|result| serde_json::from_value::<ArtifactImage>(result).ok())
            .filter(|image| sent.is_reported_by(image))
            .ok_or_else(|| unknown("the operation receipt does not describe this image".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_types_come_from_the_leading_bytes_and_each_type_has_its_limit() {
        let png = [
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a][..],
            &[0; 8],
        ]
        .concat();
        assert_eq!(artifact_image_type(&png), Ok("image/png"));
        for svg in [
            "<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
            "\u{feff}<?xml version=\"1.0\"?>\n<!-- drawn -->\n<!DOCTYPE svg>\n<svg>",
        ] {
            assert_eq!(
                artifact_image_type(svg.as_bytes()),
                Ok(SVG_MEDIA_TYPE),
                "{svg}"
            );
        }
        for not_an_image in [
            &b""[..],
            b"plain text",
            b"<svgx>",
            b"<html><svg></svg></html>",
            b"<!-- unterminated <svg>",
            b"\xff\xfe<\x00s\x00v\x00g\x00",
        ] {
            assert!(artifact_image_type(not_an_image).is_err());
        }
        let large_svg = format!("<svg>{}</svg>", " ".repeat(MAX_SVG_BYTES));
        assert!(
            artifact_image_type(large_svg.as_bytes())
                .unwrap_err()
                .contains("an SVG at most 2 MiB")
        );
        let mut large_png = png.clone();
        large_png.resize(MAX_SVG_BYTES + 1, 0);
        assert_eq!(artifact_image_type(&large_png), Ok("image/png"));
        large_png.resize(MAX_IMAGE_BYTES + 1, 0);
        assert!(artifact_image_type(&large_png).is_err());
    }
}

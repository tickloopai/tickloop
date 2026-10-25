use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ImageMediaType {
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "image/jpeg")]
    Jpeg,
    #[serde(rename = "image/webp")]
    Webp,
}

impl ImageMediaType {
    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }
}

/// An image ready to be sent to a model: base64 data in a model-accepted format and size.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Image {
    pub media_type: ImageMediaType,
    pub data: String,
    pub width: u32,
    pub height: u32,
}

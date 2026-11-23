//! Image preparation and tool-result fitting, independent of a harness.

mod exif;
mod fit;
mod native;

use std::borrow::Cow;

use async_trait::async_trait;
use base64::{
    DecodeError, Engine, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig, general_purpose::STANDARD},
};
use bytes::Bytes;
use pi_rs_ai::{ImageContent, ImageContentKind, Model};
use serde::{Deserialize, Serialize};

pub use exif::exif_orientation;
pub use fit::{
    FIT_IMAGES_EXTENSION_NAME, FittedImage, FittedImages, fit_image, fit_images, fit_tool_result,
};
pub use native::NativeImageProcessor;

pub const INLINE_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageLimits {
    pub max_width: u64,
    pub max_height: u64,
    /// Size of the base64, not the decoded file.
    pub max_bytes: u64,
}

pub const DEFAULT_IMAGE_LIMITS: ImageLimits = ImageLimits {
    max_width: 2000,
    max_height: 2000,
    max_bytes: 4_718_592,
};

impl Default for ImageLimits {
    fn default() -> Self {
        DEFAULT_IMAGE_LIMITS
    }
}

pub fn image_limits(model: Option<&Model>) -> ImageLimits {
    let Some(resize) = model
        .and_then(|model| model.input_limits.as_ref())
        .and_then(|inputs| inputs.images.as_ref())
        .and_then(|images| images.resize.as_ref())
    else {
        return DEFAULT_IMAGE_LIMITS;
    };
    ImageLimits {
        max_width: resize.max_width.unwrap_or(DEFAULT_IMAGE_LIMITS.max_width),
        max_height: resize.max_height.unwrap_or(DEFAULT_IMAGE_LIMITS.max_height),
        max_bytes: resize.max_bytes.unwrap_or(DEFAULT_IMAGE_LIMITS.max_bytes),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImageResize {
    pub from: ImageSize,
    pub to: ImageSize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedImage {
    pub data: String,
    pub mime_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resized: Option<ImageResize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub converted_from: Option<String>,
}

impl From<PreparedImage> for ImageContent {
    fn from(image: PreparedImage) -> Self {
        Self {
            kind: ImageContentKind::Image,
            data: image.data,
            mime_type: image.mime_type,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ImageProcessingError {
    #[error(transparent)]
    Worker(#[from] tokio::task::JoinError),
    #[error(transparent)]
    Runtime(#[from] tokio::runtime::TryCurrentError),
    #[error("{0}")]
    Processor(String),
}

#[async_trait]
pub trait ImageProcessor: Send + Sync {
    /// Returns `None` if the bytes cannot be decoded or made to fit. `Bytes` lets a worker take
    /// ownership without copying, or share existing storage with the caller.
    async fn prepare(
        &self,
        bytes: Bytes,
        mime_type: &str,
        limits: ImageLimits,
    ) -> Result<Option<PreparedImage>, ImageProcessingError>;
}

pub fn prepared_notes(image: &PreparedImage) -> Vec<String> {
    let mut notes = Vec::new();
    if let Some(original) = &image.converted_from {
        notes.push(format!("Converted from {original} to {}.", image.mime_type));
    }
    if let Some(ImageResize { from, to }) = image.resized {
        let scale = f64::from(from.width) / f64::from(to.width);
        // Rust's decimal formatting rounds ties to even; JS toFixed rounds a positive tie up.
        // Exact half-centesimal ties representable in binary are odd multiples of 1/8.
        let scale = if scale * 8.0 % 2.0 == 1.0 {
            (scale * 100.0).round() / 100.0
        } else {
            scale
        };
        notes.push(format!("Resized from {}x{} to {}x{}. Multiply coordinates by {scale:.2} to map them to the original.", from.width, from.height, to.width, to.height));
    }
    notes
}

pub const fn base64_length(bytes: u64) -> u64 {
    bytes.div_ceil(3).saturating_mul(4)
}

pub fn to_base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

pub fn from_base64(data: &str) -> Result<Vec<u8>, DecodeError> {
    const ATOB: GeneralPurpose = GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    let is_space = |b| matches!(b, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ');
    let data = if data.bytes().any(is_space) {
        Cow::Owned(data.bytes().filter(|&b| !is_space(b)).collect::<Vec<_>>())
    } else {
        Cow::Borrowed(data.as_bytes())
    };
    if !data.len().is_multiple_of(4) && data.contains(&b'=') {
        return Err(DecodeError::InvalidPadding);
    }
    ATOB.decode(data)
}

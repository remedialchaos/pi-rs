//! Detection and normalization of image blocks returned by tool results. Ported
//! from `utils/tool-result-images.ts` on top of the existing `image_process`,
//! `image_convert` and `image_resize` utilities.
//!
//! Tools that produce images themselves hand back arbitrary base64 payloads.
//! Upstream normalizes them once as they enter session history, because an
//! oversized or unsupported image would otherwise make every later provider
//! request fail.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use pi_rs_ai::{
    ImageContent, ImageContentKind, InputContent, ModelImageResizeOptions, TextContent, TextKind,
};
use serde::{Deserialize, Serialize};

use super::image_process::{
    ProcessImageOptions, ProcessImageResult, normalize_supported_image_mime_type, process_image,
};
use super::image_resize::ImageResizeOptions;

/// A text or image block in a tool result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(TextContent),
    Image(ImageContent),
}

impl From<InputContent> for ToolResultContent {
    fn from(content: InputContent) -> Self {
        match content {
            InputContent::Text(text) => Self::Text(text),
            InputContent::Image(image) => Self::Image(image),
        }
    }
}

impl From<ToolResultContent> for InputContent {
    fn from(content: ToolResultContent) -> Self {
        match content {
            ToolResultContent::Text(text) => Self::Text(text),
            ToolResultContent::Image(image) => Self::Image(image),
        }
    }
}

/// A decoded tool-produced image: base64 payload plus MIME type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolImageResult {
    pub data: String,
    pub mime_type: String,
}

impl ToolImageResult {
    /// Build from raw bytes, base64-encoding them.
    pub fn from_bytes(bytes: &[u8], mime_type: impl Into<String>) -> Self {
        Self {
            data: STANDARD.encode(bytes),
            mime_type: mime_type.into(),
        }
    }

    /// Decode a `data:<mime>;base64,<payload>` URL.
    pub fn from_data_url(url: &str) -> Option<Self> {
        let rest = url.strip_prefix("data:")?;
        let (header, data) = rest.split_once(',')?;
        let mime_type = header.strip_suffix(";base64")?;
        if mime_type.is_empty() || data.is_empty() {
            return None;
        }
        STANDARD.decode(data).ok()?;
        Some(Self {
            data: data.to_string(),
            mime_type: mime_type.to_string(),
        })
    }

    /// Render as a `data:` URL suitable for browser/image consumers.
    pub fn to_data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime_type, self.data)
    }

    /// Decode the base64 payload, returning `None` for invalid input.
    pub fn decode(&self) -> Option<Vec<u8>> {
        STANDARD.decode(&self.data).ok()
    }

    /// Whether the MIME type is one of the supported inline image formats.
    pub fn is_supported_mime(&self) -> bool {
        normalize_supported_image_mime_type(&self.mime_type).is_some()
    }
}

/// Validate and normalize a MIME type, allowing parameters such as `charset`.
pub fn validate_mime(mime: &str) -> Result<String, String> {
    normalize_supported_image_mime_type(mime)
        .map(str::to_string)
        .ok_or_else(|| format!("Unsupported image MIME type: {mime}"))
}

/// True for an image content block.
pub fn is_image_block(block: &ToolResultContent) -> bool {
    matches!(block, ToolResultContent::Image(_))
}

/// Extract tool-produced images from a result, in order.
pub fn image_results(content: &[ToolResultContent]) -> Vec<ToolImageResult> {
    content
        .iter()
        .filter_map(|block| match block {
            ToolResultContent::Image(image) => Some(ToolImageResult {
                data: image.data.clone(),
                mime_type: image.mime_type.clone(),
            }),
            ToolResultContent::Text(_) => None,
        })
        .collect()
}

/// Convert a tool image result to model input content.
pub fn image_result_to_input(result: &ToolImageResult) -> InputContent {
    InputContent::Image(ImageContent {
        kind: ImageContentKind::Image,
        data: result.data.clone(),
        mime_type: result.mime_type.clone(),
    })
}

/// Options for [`normalize_tool_result_images`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NormalizeToolResultImagesOptions {
    /// Whether oversized images are resized. Defaults to true.
    pub auto_resize_images: Option<bool>,
    /// Model-specific resize profile; conservative defaults when omitted.
    pub resize_options: Option<ImageResizeOptions>,
}

impl NormalizeToolResultImagesOptions {
    /// Adapt a `pi_rs_ai` model resize profile to the local image utilities.
    pub fn from_model_options(options: &ModelImageResizeOptions, auto_resize_images: bool) -> Self {
        fn u64_to_u32(value: Option<u64>) -> Option<u32> {
            value.and_then(|value| u32::try_from(value).ok())
        }
        Self {
            auto_resize_images: Some(auto_resize_images),
            resize_options: Some(ImageResizeOptions {
                max_width: u64_to_u32(options.max_width)
                    .unwrap_or(super::image_resize::DEFAULT_MAX_WIDTH),
                max_height: u64_to_u32(options.max_height)
                    .unwrap_or(super::image_resize::DEFAULT_MAX_HEIGHT),
                max_bytes: options.max_bytes.map_or(
                    super::image_resize::DEFAULT_MAX_BYTES,
                    |value| {
                        usize::try_from(value).unwrap_or(super::image_resize::DEFAULT_MAX_BYTES)
                    },
                ),
                jpeg_quality: options
                    .jpeg_quality
                    .unwrap_or(super::image_resize::DEFAULT_JPEG_QUALITY),
            }),
        }
    }
}

fn text_block(text: impl Into<String>) -> ToolResultContent {
    ToolResultContent::Text(TextContent {
        kind: TextKind::Text,
        text: text.into(),
        ..TextContent::default()
    })
}

/// Normalize image blocks in a tool result. Surrounding text keeps its position;
/// a conversion/resize hint is inserted immediately after the image it describes.
/// Images that cannot be processed are passed through unchanged so tool output is
/// never silently deleted.
pub fn normalize_tool_result_images(
    content: &[ToolResultContent],
    options: &NormalizeToolResultImagesOptions,
) -> Vec<ToolResultContent> {
    if !content.iter().any(is_image_block) {
        return content.to_vec();
    }
    let auto_resize = options.auto_resize_images.unwrap_or(true);
    let mut normalized = Vec::with_capacity(content.len());
    let mut changed = false;
    for block in content {
        let ToolResultContent::Image(image) = block else {
            normalized.push(block.clone());
            continue;
        };
        let bytes = match STANDARD.decode(&image.data) {
            Ok(bytes) => bytes,
            Err(_) => {
                normalized.push(block.clone());
                continue;
            }
        };
        let processed = process_image(
            &bytes,
            &image.mime_type,
            ProcessImageOptions {
                auto_resize_images: Some(auto_resize),
                resize_options: options.resize_options,
            },
        );
        let ProcessImageResult::Ok {
            data,
            mime_type,
            hints,
        } = processed
        else {
            normalized.push(block.clone());
            continue;
        };
        if data == image.data && mime_type == image.mime_type && hints.is_empty() {
            normalized.push(block.clone());
            continue;
        }
        normalized.push(ToolResultContent::Image(ImageContent {
            kind: ImageContentKind::Image,
            data,
            mime_type,
        }));
        if !hints.is_empty() {
            normalized.push(text_block(hints.join("\n")));
        }
        changed = true;
    }
    if changed {
        normalized
    } else {
        content.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat};

    fn png_bytes(width: u32, height: u32) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        DynamicImage::new_rgba8(width, height)
            .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        Ok(bytes)
    }

    fn image_block(bytes: &[u8], mime: &str) -> ToolResultContent {
        ToolResultContent::Image(ImageContent {
            kind: ImageContentKind::Image,
            data: STANDARD.encode(bytes),
            mime_type: mime.into(),
        })
    }

    #[test]
    fn no_images_returns_unchanged_content() {
        let content = vec![text_block("no images here")];
        assert_eq!(
            normalize_tool_result_images(&content, &NormalizeToolResultImagesOptions::default()),
            content
        );
    }

    #[test]
    fn tiny_png_is_unchanged() -> Result<(), String> {
        let content = vec![image_block(&png_bytes(1, 1)?, "image/png")];
        assert_eq!(
            normalize_tool_result_images(&content, &NormalizeToolResultImagesOptions::default()),
            content
        );
        Ok(())
    }

    #[test]
    fn oversized_image_is_resized_with_dimension_hint() -> Result<(), String> {
        let content = vec![image_block(&png_bytes(2400, 4800)?, "image/png")];
        let normalized =
            normalize_tool_result_images(&content, &NormalizeToolResultImagesOptions::default());
        assert_eq!(normalized.len(), 2);
        match &normalized[0] {
            ToolResultContent::Image(image) => assert_eq!(image.mime_type, "image/png"),
            _ => return Err("expected image".to_string()),
        }
        match &normalized[1] {
            ToolResultContent::Text(text) => assert!(text.text.contains("original 2400x4800")),
            _ => return Err("expected hint text".to_string()),
        }
        Ok(())
    }

    #[test]
    fn auto_resize_can_be_disabled() -> Result<(), String> {
        let content = vec![image_block(&png_bytes(2400, 4800)?, "image/png")];
        let options = NormalizeToolResultImagesOptions {
            auto_resize_images: Some(false),
            ..NormalizeToolResultImagesOptions::default()
        };
        assert_eq!(normalize_tool_result_images(&content, &options), content);
        Ok(())
    }

    #[test]
    fn unsupported_format_is_converted_even_without_resize() -> Result<(), String> {
        let mut bytes = Vec::new();
        DynamicImage::new_rgba8(1, 1)
            .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Bmp)
            .map_err(|e| e.to_string())?;
        let content = vec![image_block(&bytes, "image/bmp")];
        let options = NormalizeToolResultImagesOptions {
            auto_resize_images: Some(false),
            ..NormalizeToolResultImagesOptions::default()
        };
        let normalized = normalize_tool_result_images(&content, &options);
        assert_eq!(normalized.len(), 2);
        match &normalized[0] {
            ToolResultContent::Image(image) => assert_eq!(image.mime_type, "image/png"),
            _ => return Err("expected image".to_string()),
        }
        match &normalized[1] {
            ToolResultContent::Text(text) => {
                assert_eq!(text.text, "[Image converted from image/bmp to image/png.]")
            }
            _ => return Err("expected hint text".to_string()),
        }
        Ok(())
    }

    #[test]
    fn undecodable_image_is_preserved() {
        let content = vec![ToolResultContent::Image(ImageContent {
            kind: ImageContentKind::Image,
            data: "bm90LWFuLWltYWdl".into(),
            mime_type: "image/png".into(),
        })];
        assert_eq!(
            normalize_tool_result_images(&content, &NormalizeToolResultImagesOptions::default()),
            content
        );
    }

    #[test]
    fn text_order_is_preserved() -> Result<(), String> {
        let content = vec![
            text_block("before"),
            image_block(&png_bytes(2400, 100)?, "image/png"),
            text_block("after"),
        ];
        let normalized =
            normalize_tool_result_images(&content, &NormalizeToolResultImagesOptions::default());
        assert_eq!(normalized.len(), 4);
        assert_eq!(normalized[0], content[0]);
        assert_eq!(normalized[3], content[2]);
        Ok(())
    }

    #[test]
    fn data_url_round_trip_and_mime_validation() -> Result<(), String> {
        let bytes = png_bytes(1, 1)?;
        let result = ToolImageResult::from_bytes(&bytes, "image/png");
        assert!(result.is_supported_mime());
        assert_eq!(result.decode().as_deref(), Some(bytes.as_slice()));
        let url = result.to_data_url();
        assert_eq!(ToolImageResult::from_data_url(&url), Some(result));
        assert_eq!(validate_mime("IMAGE/JPG")?, "image/jpeg");
        assert!(validate_mime("image/bmp").is_err());
        assert_eq!(
            image_results(&[ToolResultContent::Image(ImageContent {
                kind: ImageContentKind::Image,
                data: STANDARD.encode(&bytes),
                mime_type: "image/png".into(),
            })])
            .len(),
            1
        );
        Ok(())
    }

    #[test]
    fn model_resize_options_are_adapted() -> Result<(), String> {
        let model = ModelImageResizeOptions {
            max_width: Some(1600),
            max_height: Some(1200),
            max_bytes: Some(1024),
            jpeg_quality: Some(75),
        };
        let options = NormalizeToolResultImagesOptions::from_model_options(&model, true);
        let resize = options.resize_options.ok_or("missing resize options")?;
        assert_eq!(resize.max_width, 1600);
        assert_eq!(resize.max_height, 1200);
        assert_eq!(resize.max_bytes, 1024);
        assert_eq!(resize.jpeg_quality, 75);
        Ok(())
    }
}

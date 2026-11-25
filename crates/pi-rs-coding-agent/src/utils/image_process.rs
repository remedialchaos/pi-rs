//! Image preparation for inline model content, ported from `utils/image-process.ts`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};

use super::image_convert::convert_bytes_to_png;
use super::image_resize::{ImageResizeOptions, ResizedImage, format_dimension_note, resize_image};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessImageOptions {
    /// Whether to resize images to provider limits. Defaults to true.
    pub auto_resize_images: Option<bool>,
    pub resize_options: Option<ImageResizeOptions>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessImageResult {
    Ok {
        data: String,
        mime_type: String,
        hints: Vec<String>,
    },
    Err {
        message: String,
    },
}

const CONVERSION_FAILURE: &str =
    "[Image omitted: could not be converted to a supported inline image format.]";
const RESIZE_FAILURE: &str =
    "[Image omitted: could not be resized below the inline image size limit.]";

pub fn normalize_supported_image_mime_type(mime: &str) -> Option<&'static str> {
    let base = mime.split(';').next().unwrap_or(mime).trim().to_lowercase();
    match base.as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

pub fn conversion_hint(from: Option<&str>, to: &str) -> Option<String> {
    match from {
        Some(from) if from != to => Some(format!("[Image converted from {from} to {to}.]")),
        _ => None,
    }
}

struct NormalizedImage {
    bytes: Vec<u8>,
    mime_type: String,
    converted_from: Option<String>,
}

fn normalize_image(bytes: &[u8], mime_type: &str) -> Result<NormalizedImage, String> {
    if let Some(mime) = normalize_supported_image_mime_type(mime_type) {
        return Ok(NormalizedImage {
            bytes: bytes.to_vec(),
            mime_type: mime.to_string(),
            converted_from: None,
        });
    }
    let png = convert_bytes_to_png(bytes)?;
    let base = mime_type
        .split(';')
        .next()
        .unwrap_or(mime_type)
        .trim()
        .to_lowercase();
    Ok(NormalizedImage {
        bytes: png,
        mime_type: "image/png".to_string(),
        converted_from: Some(base),
    })
}

fn hints(normalized: &NormalizedImage, resized: Option<&ResizedImage>) -> Vec<String> {
    let mime = resized.map_or(normalized.mime_type.as_str(), |result| {
        result.mime_type.as_str()
    });
    let mut result = Vec::new();
    if let Some(hint) = conversion_hint(normalized.converted_from.as_deref(), mime) {
        result.push(hint);
    }
    if let Some(hint) = resized.and_then(format_dimension_note) {
        result.push(hint);
    }
    result
}

/// Process image bytes into a supported, size-bounded inline payload.
pub fn process_image(
    bytes: &[u8],
    mime_type: &str,
    options: ProcessImageOptions,
) -> ProcessImageResult {
    let normalized = match normalize_image(bytes, mime_type) {
        Ok(image) => image,
        Err(_) => {
            return ProcessImageResult::Err {
                message: CONVERSION_FAILURE.to_string(),
            };
        }
    };

    if options.auto_resize_images.unwrap_or(true) {
        let options = options.resize_options.unwrap_or_default();
        let resized = match resize_image(&normalized.bytes, &normalized.mime_type, options) {
            Ok(Some(result)) => result,
            _ => {
                return ProcessImageResult::Err {
                    message: RESIZE_FAILURE.to_string(),
                };
            }
        };
        return ProcessImageResult::Ok {
            data: resized.data.clone(),
            mime_type: resized.mime_type.clone(),
            hints: hints(&normalized, Some(&resized)),
        };
    }

    ProcessImageResult::Ok {
        data: STANDARD.encode(&normalized.bytes),
        mime_type: normalized.mime_type.clone(),
        hints: hints(&normalized, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat};

    fn png_bytes() -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        DynamicImage::new_rgba8(2, 2)
            .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        Ok(bytes)
    }

    #[test]
    fn mime_normalization_maps_jpg_and_parameters() {
        assert_eq!(
            normalize_supported_image_mime_type("IMAGE/JPG"),
            Some("image/jpeg")
        );
        assert_eq!(
            normalize_supported_image_mime_type("image/png; charset=x"),
            Some("image/png")
        );
        assert_eq!(normalize_supported_image_mime_type("image/bmp"), None);
    }

    #[test]
    fn conversion_hint_uses_exact_message() {
        assert_eq!(
            conversion_hint(Some("image/bmp"), "image/png").as_deref(),
            Some("[Image converted from image/bmp to image/png.]")
        );
        assert_eq!(conversion_hint(Some("image/png"), "image/png"), None);
        assert_eq!(conversion_hint(None, "image/png"), None);
    }

    #[test]
    fn supported_png_is_inlined_without_resize() -> Result<(), String> {
        let bytes = png_bytes()?;
        let result = process_image(&bytes, "image/png", ProcessImageOptions::default());
        match result {
            ProcessImageResult::Ok {
                data,
                mime_type,
                hints,
            } => {
                assert_eq!(data, STANDARD.encode(&bytes));
                assert_eq!(mime_type, "image/png");
                assert!(hints.is_empty());
            }
            ProcessImageResult::Err { message } => return Err(message),
        }
        Ok(())
    }

    #[test]
    fn bmp_is_converted_to_png_with_hint() -> Result<(), String> {
        let mut bytes = Vec::new();
        DynamicImage::new_rgba8(2, 2)
            .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Bmp)
            .map_err(|e| e.to_string())?;
        let result = process_image(
            &bytes,
            "image/bmp",
            ProcessImageOptions {
                auto_resize_images: Some(false),
                ..ProcessImageOptions::default()
            },
        );
        match result {
            ProcessImageResult::Ok {
                mime_type, hints, ..
            } => {
                assert_eq!(mime_type, "image/png");
                assert_eq!(
                    hints,
                    vec!["[Image converted from image/bmp to image/png.]"]
                );
            }
            ProcessImageResult::Err { message } => return Err(message),
        }
        Ok(())
    }

    #[test]
    fn impossible_size_limit_uses_exact_error() -> Result<(), String> {
        let bytes = png_bytes()?;
        let result = process_image(
            &bytes,
            "image/png",
            ProcessImageOptions {
                auto_resize_images: Some(true),
                resize_options: Some(ImageResizeOptions {
                    max_bytes: 1,
                    ..ImageResizeOptions::default()
                }),
            },
        );
        assert_eq!(
            result,
            ProcessImageResult::Err {
                message: "[Image omitted: could not be resized below the inline image size limit.]"
                    .to_string()
            }
        );
        Ok(())
    }

    #[test]
    fn undecodable_unsupported_bytes_uses_conversion_error() {
        assert_eq!(
            process_image(b"not an image", "image/bmp", ProcessImageOptions::default()),
            ProcessImageResult::Err {
                message:
                    "[Image omitted: could not be converted to a supported inline image format.]"
                        .to_string()
            }
        );
    }
}

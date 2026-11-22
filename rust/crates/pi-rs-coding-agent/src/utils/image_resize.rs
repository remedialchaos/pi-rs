//! Image resizing and size-limit enforcement, ported from `utils/image-resize-core.ts` and
//! `utils/image-resize.ts`. Rust has no worker boundary, so the worker reply tag is kept only for
//! protocol compatibility with callers that tag messages.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::{FilterType, resize};
use image::{DynamicImage, GenericImageView};
use serde::{Deserialize, Serialize};

pub const IMAGE_RESIZE_WORKER_RESPONSE_TYPE: &str = "pi:image-resize-response";
pub const DEFAULT_MAX_WIDTH: u32 = 2000;
pub const DEFAULT_MAX_HEIGHT: u32 = 2000;
/// 4.5 MiB of base64 payload, below the 5 MB provider limit.
pub const DEFAULT_MAX_BYTES: usize = 4_718_592;
pub const DEFAULT_JPEG_QUALITY: u8 = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageResizeOptions {
    pub max_width: u32,
    pub max_height: u32,
    pub max_bytes: usize,
    pub jpeg_quality: u8,
}

impl Default for ImageResizeOptions {
    fn default() -> Self {
        Self {
            max_width: DEFAULT_MAX_WIDTH,
            max_height: DEFAULT_MAX_HEIGHT,
            max_bytes: DEFAULT_MAX_BYTES,
            jpeg_quality: DEFAULT_JPEG_QUALITY,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResizedImage {
    /// base64 payload
    pub data: String,
    pub mime_type: String,
    pub original_width: u32,
    pub original_height: u32,
    pub width: u32,
    pub height: u32,
    pub was_resized: bool,
}

struct EncodedCandidate {
    data: String,
    encoded_size: usize,
    mime_type: &'static str,
}

fn encode_candidate(
    image: &DynamicImage,
    mime: &'static str,
    quality: u8,
) -> Result<EncodedCandidate, String> {
    let mut bytes = Vec::new();
    if mime == "image/png" {
        image
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .map_err(|error| error.to_string())?;
    } else {
        JpegEncoder::new_with_quality(&mut bytes, quality)
            .encode_image(image)
            .map_err(|error| error.to_string())?;
    }
    let data = STANDARD.encode(bytes);
    Ok(EncodedCandidate {
        encoded_size: data.len(),
        data,
        mime_type: mime,
    })
}

fn quality_steps(quality: u8) -> Vec<u8> {
    let mut steps = Vec::with_capacity(5);
    for candidate in [quality, 85, 70, 55, 40] {
        if !steps.contains(&candidate) {
            steps.push(candidate);
        }
    }
    steps
}

/// Resize and re-encode until the base64 payload is strictly smaller than `max_bytes`.
/// `Ok(None)` mirrors upstream's `null` for undecodable data or impossible size limits.
pub fn resize_image(
    bytes: &[u8],
    mime_type: &str,
    options: ImageResizeOptions,
) -> Result<Option<ResizedImage>, String> {
    let decoded = match image::load_from_memory(bytes) {
        Ok(image) => image,
        Err(_) => return Ok(None),
    };
    let image = super::exif_orientation::apply_exif_orientation(decoded, bytes);
    let (original_width, original_height) = image.dimensions();
    let format = mime_type.split('/').nth(1).unwrap_or("png");
    let fallback_mime = format!("image/{format}");
    let effective_mime = if mime_type.is_empty() {
        fallback_mime.as_str()
    } else {
        mime_type
    };
    let input_base64_size = bytes.len().div_ceil(3) * 4;

    if original_width <= options.max_width
        && original_height <= options.max_height
        && input_base64_size < options.max_bytes
    {
        return Ok(Some(ResizedImage {
            data: STANDARD.encode(bytes),
            mime_type: effective_mime.to_string(),
            original_width,
            original_height,
            width: original_width,
            height: original_height,
            was_resized: false,
        }));
    }

    let mut target_width = original_width;
    let mut target_height = original_height;
    if target_width > options.max_width {
        target_height = ((u64::from(target_height) * u64::from(options.max_width))
            / u64::from(target_width)) as u32;
        target_width = options.max_width;
    }
    if target_height > options.max_height {
        target_width = ((u64::from(target_width) * u64::from(options.max_height))
            / u64::from(target_height)) as u32;
        target_height = options.max_height;
    }

    let steps = quality_steps(options.jpeg_quality);
    let mut current_width = target_width.max(1);
    let mut current_height = target_height.max(1);
    loop {
        let resized = DynamicImage::ImageRgba8(resize(
            &image,
            current_width,
            current_height,
            FilterType::Lanczos3,
        ));
        let png = encode_candidate(&resized, "image/png", options.jpeg_quality)?;
        let jpegs = steps
            .iter()
            .map(|quality| encode_candidate(&resized, "image/jpeg", *quality))
            .collect::<Result<Vec<_>, String>>()?;
        let mut candidates = Vec::with_capacity(jpegs.len() + 1);
        match jpegs.first() {
            Some(first) if first.encoded_size < png.encoded_size => {
                candidates.push(first);
                candidates.push(&png);
                candidates.extend(jpegs.iter().skip(1));
            }
            _ => {
                candidates.push(&png);
                candidates.extend(jpegs.iter());
            }
        }
        for candidate in candidates {
            if candidate.encoded_size < options.max_bytes {
                return Ok(Some(ResizedImage {
                    data: candidate.data.clone(),
                    mime_type: candidate.mime_type.to_string(),
                    original_width,
                    original_height,
                    width: current_width,
                    height: current_height,
                    was_resized: true,
                }));
            }
        }

        if current_width == 1 && current_height == 1 {
            break;
        }
        let next_width = (current_width * 3 / 4).max(1);
        let next_height = (current_height * 3 / 4).max(1);
        if next_width == current_width && next_height == current_height {
            break;
        }
        current_width = next_width;
        current_height = next_height;
    }
    Ok(None)
}

/// Dimension hint shown to the model so image coordinates can be mapped to the original.
pub fn format_dimension_note(result: &ResizedImage) -> Option<String> {
    if !result.was_resized {
        return None;
    }
    let scale = f64::from(result.original_width) / f64::from(result.width);
    Some(format!(
        "[Image: original {}x{}, displayed at {}x{}. Multiply coordinates by {scale:.2} to map to original image.]",
        result.original_width, result.original_height, result.width, result.height
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        DynamicImage::new_rgba8(width, height)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .map_err(|e| e.to_string())?;
        Ok(bytes)
    }

    #[test]
    fn image_within_limits_is_returned_unchanged() -> Result<(), String> {
        let bytes = png(2, 2)?;
        let result = resize_image(&bytes, "image/png", ImageResizeOptions::default())?
            .ok_or("expected result")?;
        assert!(!result.was_resized);
        assert_eq!(result.data, STANDARD.encode(&bytes));
        assert_eq!((result.width, result.height), (2, 2));
        assert_eq!(result.original_width, 2);
        Ok(())
    }

    #[test]
    fn dimensions_are_capped_at_max_width_and_height() -> Result<(), String> {
        let bytes = png(100, 100)?;
        let options = ImageResizeOptions {
            max_width: 50,
            max_height: 50,
            ..ImageResizeOptions::default()
        };
        let result = resize_image(&bytes, "image/png", options)?.ok_or("expected result")?;
        assert!(result.was_resized);
        assert_eq!((result.width, result.height), (50, 50));
        assert_eq!((result.original_width, result.original_height), (100, 100));
        Ok(())
    }

    #[test]
    fn one_byte_limit_returns_none() -> Result<(), String> {
        let bytes = png(20, 20)?;
        let options = ImageResizeOptions {
            max_bytes: 1,
            ..ImageResizeOptions::default()
        };
        assert_eq!(resize_image(&bytes, "image/png", options)?, None);
        Ok(())
    }

    #[test]
    fn base64_size_check_is_strict() -> Result<(), String> {
        let bytes = png(2, 2)?;
        let exact = bytes.len().div_ceil(3) * 4;
        let fits = resize_image(
            &bytes,
            "image/png",
            ImageResizeOptions {
                max_bytes: exact + 1,
                ..ImageResizeOptions::default()
            },
        )?
        .ok_or("expected result")?;
        assert!(!fits.was_resized);
        let does_not_fit = resize_image(
            &bytes,
            "image/png",
            ImageResizeOptions {
                max_bytes: exact,
                ..ImageResizeOptions::default()
            },
        )?
        .ok_or("expected result")?;
        assert!(does_not_fit.was_resized);
        Ok(())
    }

    #[test]
    fn noisy_image_prefers_jpeg_when_png_does_not_fit() -> Result<(), String> {
        let mut image = image::RgbaImage::new(200, 200);
        let mut seed = 17u32;
        for y in 0..200 {
            for x in 0..200 {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                let value = (seed >> 16) as u8;
                image.put_pixel(x, y, image::Rgba([value, value ^ 0x55, value >> 1, 255]));
            }
        }
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .map_err(|e| e.to_string())?;
        let options = ImageResizeOptions {
            max_bytes: 100_000,
            ..ImageResizeOptions::default()
        };
        let result = resize_image(&bytes, "image/png", options)?.ok_or("expected result")?;
        assert_eq!(result.mime_type, "image/jpeg");
        assert!(result.data.len() < 100_000);
        Ok(())
    }

    #[test]
    fn shrink_loop_reaches_one_by_one_when_needed() -> Result<(), String> {
        let mut image = image::RgbaImage::new(64, 64);
        for y in 0..64 {
            for x in 0..64 {
                image.put_pixel(x, y, image::Rgba([(x * 3) as u8, (y * 5) as u8, 0, 255]));
            }
        }
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .map_err(|e| e.to_string())?;
        let options = ImageResizeOptions {
            max_bytes: 500,
            ..ImageResizeOptions::default()
        };
        let result = resize_image(&bytes, "image/png", options)?.ok_or("expected result")?;
        assert!(result.width <= 64 && result.height <= 64);
        assert!(result.data.len() < 500);
        Ok(())
    }

    #[test]
    fn dimension_note_has_two_decimal_scale() {
        let result = ResizedImage {
            data: String::new(),
            mime_type: "image/png".to_string(),
            original_width: 2000,
            original_height: 1000,
            width: 1000,
            height: 500,
            was_resized: true,
        };
        let note = format_dimension_note(&result).unwrap_or_default();
        assert!(note.contains("original 2000x1000"));
        assert!(note.contains("displayed at 1000x500"));
        assert!(note.contains("2.00"));
        let mut unchanged = result;
        unchanged.was_resized = false;
        assert_eq!(format_dimension_note(&unchanged), None);
    }

    #[test]
    fn undecodable_data_is_null() {
        assert_eq!(
            resize_image(b"junk", "image/png", ImageResizeOptions::default()),
            Ok(None)
        );
    }
}

//! Image-to-PNG conversion, ported from `utils/image-convert.ts`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::{DynamicImage, ImageFormat};

use super::exif_orientation::apply_exif_orientation;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PngData {
    pub data: String,
    pub mime_type: String,
}

fn base_mime_type(mime: &str) -> &str {
    mime.split(';').next().unwrap_or(mime).trim()
}

pub fn encode_png(image: &DynamicImage) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok(bytes)
}

/// Decode any format supported by the `image` crate, apply EXIF orientation, and encode PNG.
pub fn convert_bytes_to_png(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let image = image::load_from_memory(bytes).map_err(|error| error.to_string())?;
    let image = apply_exif_orientation(image, bytes);
    encode_png(&image)
}

/// Upstream `convertToPng`: PNG data passes through unchanged; other formats are transcoded.
pub fn convert_to_png(base64_data: &str, mime_type: &str) -> Result<Option<PngData>, String> {
    let normalized = base_mime_type(mime_type);
    if normalized.eq_ignore_ascii_case("image/png") {
        return Ok(Some(PngData {
            data: base64_data.to_string(),
            mime_type: mime_type.to_string(),
        }));
    }
    let bytes = STANDARD
        .decode(base64_data)
        .map_err(|error| error.to_string())?;
    let png = convert_bytes_to_png(&bytes)?;
    Ok(Some(PngData {
        data: STANDARD.encode(png),
        mime_type: "image/png".to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    fn jpeg_with_orientation(width: u32, height: u32, orientation: u16) -> Result<Vec<u8>, String> {
        let image = DynamicImage::new_rgba8(width, height);
        let mut jpeg = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut jpeg), ImageFormat::Jpeg)
            .map_err(|e| e.to_string())?;
        let mut tiff = vec![b'I', b'I'];
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&orientation.to_le_bytes());
        tiff.extend_from_slice(&0u16.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        let mut result = vec![0xff, 0xd8];
        result.extend_from_slice(&[0xff, 0xe1]);
        result.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
        result.extend_from_slice(b"Exif\0\0");
        result.extend_from_slice(&tiff);
        result.extend_from_slice(&jpeg[2..]);
        Ok(result)
    }

    #[test]
    fn png_input_passes_through_unchanged() -> Result<(), String> {
        let result = convert_to_png("aGVsbG8=", "image/png")?;
        assert_eq!(
            result,
            Some(PngData {
                data: "aGVsbG8=".to_string(),
                mime_type: "image/png".to_string()
            })
        );
        Ok(())
    }

    #[test]
    fn jpeg_input_becomes_valid_png_base64() -> Result<(), String> {
        let mut jpeg = Vec::new();
        DynamicImage::new_rgba8(3, 2)
            .write_to(&mut std::io::Cursor::new(&mut jpeg), ImageFormat::Jpeg)
            .map_err(|e| e.to_string())?;
        let base64 = STANDARD.encode(&jpeg);
        let result = convert_to_png(&base64, "image/jpeg")?.ok_or("expected converted data")?;
        assert_eq!(result.mime_type, "image/png");
        let png = STANDARD.decode(&result.data).map_err(|e| e.to_string())?;
        assert_eq!(&png[0..4], b"\x89PNG");
        assert_eq!(
            image::load_from_memory(&png)
                .map_err(|e| e.to_string())?
                .dimensions(),
            (3, 2)
        );
        Ok(())
    }

    #[test]
    fn exif_orientation_six_is_applied_during_conversion() -> Result<(), String> {
        let jpeg = jpeg_with_orientation(2, 1, 6)?;
        let png = convert_bytes_to_png(&jpeg)?;
        assert_eq!(
            image::load_from_memory(&png)
                .map_err(|e| e.to_string())?
                .dimensions(),
            (1, 2)
        );
        Ok(())
    }

    #[test]
    fn invalid_base64_is_an_error() {
        assert!(convert_to_png("!!!not base64!!!", "image/jpeg").is_err());
    }

    #[test]
    fn undecodable_bytes_are_an_error() {
        assert!(convert_bytes_to_png(b"not an image").is_err());
    }
}

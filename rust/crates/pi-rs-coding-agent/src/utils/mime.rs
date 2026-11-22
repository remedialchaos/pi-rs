//! Image MIME sniffing, port of `packages/coding-agent/src/utils/mime.ts`.
//!
//! Only the magic-byte checks are ported; decoding/validation of the full image is
//! intentionally not done. All readers treat missing bytes as zero, matching the
//! upstream `buffer[offset] ?? 0` behavior.

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Number of leading bytes read from a file for MIME detection.
pub const IMAGE_TYPE_SNIFF_BYTES: usize = 4100;

/// PNG magic signature.
pub const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// Return the supported image MIME type for `buffer`, or `None` when the bytes do
/// not match a supported, non-animated signature.
pub fn detect_supported_image_mime_type(buffer: &[u8]) -> Option<&'static str> {
    if starts_with(buffer, &[0xff, 0xd8, 0xff]) {
        // JPEG 2000 / JPEG XT shares the prefix but uses 0xf7 as the fourth byte.
        return if buffer.get(3) == Some(&0xf7) {
            None
        } else {
            Some("image/jpeg")
        };
    }
    if starts_with(buffer, &PNG_SIGNATURE) {
        return if is_png(buffer) && !is_animated_png(buffer) {
            Some("image/png")
        } else {
            None
        };
    }
    if starts_with_ascii(buffer, 0, "GIF87a") || starts_with_ascii(buffer, 0, "GIF89a") {
        return Some("image/gif");
    }
    if starts_with_ascii(buffer, 0, "RIFF") && starts_with_ascii(buffer, 8, "WEBP") {
        return Some("image/webp");
    }
    if starts_with_ascii(buffer, 0, "BM") && is_bmp(buffer) {
        return Some("image/bmp");
    }
    None
}

/// Synchronously read up to [`IMAGE_TYPE_SNIFF_BYTES`] bytes from `file_path` and
/// detect the image MIME type from those bytes.
///
/// Returns `Err` when the file cannot be opened or read; returns `Ok(None)` when
/// the bytes do not match a supported signature.
pub fn detect_supported_image_mime_type_from_file(
    file_path: &Path,
) -> Result<Option<&'static str>, String> {
    let mut file = File::open(file_path).map_err(|error| error.to_string())?;
    let mut buffer = [0u8; IMAGE_TYPE_SNIFF_BYTES];
    let bytes_read = file.read(&mut buffer).map_err(|error| error.to_string())?;
    Ok(detect_supported_image_mime_type(&buffer[..bytes_read]))
}

/// PNG signature plus a well-formed 13-byte `IHDR` chunk header.
pub fn is_png(buffer: &[u8]) -> bool {
    buffer.len() >= 16
        && read_uint32_be(buffer, PNG_SIGNATURE.len()) == 13
        && starts_with_ascii(buffer, 12, "IHDR")
}

/// Walk PNG chunks from offset 7; `acTL` marks an animated PNG, `IDAT` ends the
/// search (a static PNG). Malformed/oversized chunk lengths end the search.
pub fn is_animated_png(buffer: &[u8]) -> bool {
    let mut offset = PNG_SIGNATURE.len();
    while offset + 8 <= buffer.len() {
        let chunk_length = read_uint32_be(buffer, offset) as usize;
        let chunk_type_offset = offset + 4;
        if starts_with_ascii(buffer, chunk_type_offset, "acTL") {
            return true;
        }
        if starts_with_ascii(buffer, chunk_type_offset, "IDAT") {
            return false;
        }

        // Use wrapping arithmetic to mirror JS number behavior for absurd lengths;
        // `read_uint32_be` is at most u32::MAX so this cannot overflow usize on 64-bit.
        let next_offset = offset + 8 + chunk_length + 4;
        if next_offset <= offset || next_offset > buffer.len() {
            return false;
        }
        offset = next_offset;
    }
    false
}

/// Validate the BMP header fields the same way upstream does.
pub fn is_bmp(buffer: &[u8]) -> bool {
    if buffer.len() < 26 {
        return false;
    }

    let declared_file_size = read_uint32_le(buffer, 2);
    let pixel_data_offset = read_uint32_le(buffer, 10);
    let dib_header_size = read_uint32_le(buffer, 14);
    if declared_file_size != 0 && declared_file_size < 26 {
        return false;
    }
    if pixel_data_offset < 14 + dib_header_size {
        return false;
    }
    if declared_file_size != 0 && pixel_data_offset >= declared_file_size {
        return false;
    }

    let (color_planes, bits_per_pixel) = if dib_header_size == 12 {
        (read_uint16_le(buffer, 22), read_uint16_le(buffer, 24))
    } else if (40..=124).contains(&dib_header_size) {
        if buffer.len() < 30 {
            return false;
        }
        (read_uint16_le(buffer, 26), read_uint16_le(buffer, 28))
    } else {
        return false;
    };

    color_planes == 1 && matches!(bits_per_pixel, 1 | 4 | 8 | 16 | 24 | 32)
}

/// Missing bytes read as zero, matching `buffer[offset] ?? 0`.
fn byte_or_zero(buffer: &[u8], offset: usize) -> u32 {
    buffer.get(offset).copied().unwrap_or(0) as u32
}

fn read_uint16_le(buffer: &[u8], offset: usize) -> u32 {
    byte_or_zero(buffer, offset) + (byte_or_zero(buffer, offset + 1) << 8)
}

fn read_uint32_be(buffer: &[u8], offset: usize) -> u32 {
    byte_or_zero(buffer, offset) * 0x0100_0000
        + (byte_or_zero(buffer, offset + 1) << 16)
        + (byte_or_zero(buffer, offset + 2) << 8)
        + byte_or_zero(buffer, offset + 3)
}

fn read_uint32_le(buffer: &[u8], offset: usize) -> u32 {
    byte_or_zero(buffer, offset)
        + (byte_or_zero(buffer, offset + 1) << 8)
        + (byte_or_zero(buffer, offset + 2) << 16)
        + byte_or_zero(buffer, offset + 3) * 0x0100_0000
}

fn starts_with(buffer: &[u8], bytes: &[u8]) -> bool {
    buffer.len() >= bytes.len() && buffer[..bytes.len()] == *bytes
}

fn starts_with_ascii(buffer: &[u8], offset: usize, text: &str) -> bool {
    if buffer.len() < offset + text.len() {
        return false;
    }
    buffer[offset..offset + text.len()] == *text.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bmp_bytes() -> Vec<u8> {
        // 58-byte BITMAPINFOHEADER BMP: "BM", size 58, pixel offset 54, DIB size 40,
        // width/height 1, planes 1, bpp 24. Matches image-process.test.ts lines 5-19.
        let mut buffer = vec![0u8; 58];
        buffer[0] = b'B';
        buffer[1] = b'M';
        buffer[2..6].copy_from_slice(&58u32.to_le_bytes());
        buffer[10..14].copy_from_slice(&54u32.to_le_bytes());
        buffer[14..18].copy_from_slice(&40u32.to_le_bytes());
        buffer[18..22].copy_from_slice(&1u32.to_le_bytes()); // width
        buffer[22..26].copy_from_slice(&1u32.to_le_bytes()); // height
        buffer[26..28].copy_from_slice(&1u16.to_le_bytes()); // planes
        buffer[28..30].copy_from_slice(&24u16.to_le_bytes()); // bits per pixel
        buffer
    }

    #[test]
    fn detects_gif_signatures() {
        assert_eq!(
            detect_supported_image_mime_type(b"GIF87a"),
            Some("image/gif")
        );
        assert_eq!(
            detect_supported_image_mime_type(b"GIF89a"),
            Some("image/gif")
        );
    }

    #[test]
    fn detects_bmp_from_magic_bytes() {
        let buffer = bmp_bytes();
        assert_eq!(detect_supported_image_mime_type(&buffer), Some("image/bmp"));
    }

    #[test]
    fn rejects_short_bmp_header() {
        let buffer = bmp_bytes();
        assert_eq!(detect_supported_image_mime_type(&buffer[..20]), None);
    }

    #[test]
    fn detects_jpeg_but_rejects_jpeg_xt() {
        assert_eq!(
            detect_supported_image_mime_type(&[0xff, 0xd8, 0xff, 0xe0]),
            Some("image/jpeg")
        );
        assert_eq!(
            detect_supported_image_mime_type(&[0xff, 0xd8, 0xff, 0xf7]),
            None
        );
    }

    #[test]
    fn detects_webp() {
        let mut buffer = vec![0u8; 16];
        buffer[..4].copy_from_slice(b"RIFF");
        buffer[8..12].copy_from_slice(b"WEBP");
        assert_eq!(
            detect_supported_image_mime_type(&buffer),
            Some("image/webp")
        );
    }

    fn append_chunk(buffer: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
        buffer.extend_from_slice(&(data.len() as u32).to_be_bytes());
        buffer.extend_from_slice(chunk_type);
        buffer.extend_from_slice(data);
        buffer.extend_from_slice(&0u32.to_be_bytes()); // CRC placeholder
    }

    #[test]
    fn detects_static_png_and_rejects_animated_png() {
        let mut png = PNG_SIGNATURE.to_vec();
        append_chunk(&mut png, b"IHDR", &[0u8; 13]);
        assert!(is_png(&png));
        assert!(!is_animated_png(&png));
        assert_eq!(detect_supported_image_mime_type(&png), Some("image/png"));

        let mut animated = png;
        append_chunk(&mut animated, b"acTL", &[]);
        assert!(is_animated_png(&animated));
        assert_eq!(detect_supported_image_mime_type(&animated), None);
    }

    #[test]
    fn readers_treat_missing_bytes_as_zero() {
        assert_eq!(read_uint16_le(&[], 0), 0);
        assert_eq!(read_uint32_be(&[1], 0), 0x0100_0000);
        assert_eq!(read_uint32_le(&[1, 2], 0), 0x0201);
        assert!(!starts_with(&[1, 2], &[1, 2, 3]));
    }

    #[test]
    fn unknown_bytes_return_none() {
        assert_eq!(detect_supported_image_mime_type(b"hello world"), None);
        assert_eq!(detect_supported_image_mime_type(&[]), None);
    }
}

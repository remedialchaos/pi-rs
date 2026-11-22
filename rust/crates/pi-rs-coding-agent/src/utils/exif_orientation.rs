//! EXIF orientation parsing and transposition, ported from `utils/exif-orientation.ts`.
//!
//! JPEG APP1 (`Exif\0\0` + TIFF) and WebP `EXIF` chunks are supported. The TIFF reader handles
//! both little- and big-endian headers and returns `None` for missing/invalid tags; callers that
//! need upstream's "default 1" behavior use `unwrap_or(1)`.

use image::{DynamicImage, RgbaImage};

fn read_u16(bytes: &[u8], position: usize, little_endian: bool) -> Option<u16> {
    let pair: [u8; 2] = bytes.get(position..position + 2)?.try_into().ok()?;
    Some(if little_endian {
        u16::from_le_bytes(pair)
    } else {
        u16::from_be_bytes(pair)
    })
}

fn read_u32(bytes: &[u8], position: usize, little_endian: bool) -> Option<u32> {
    let quad: [u8; 4] = bytes.get(position..position + 4)?.try_into().ok()?;
    Some(if little_endian {
        u32::from_le_bytes(quad)
    } else {
        u32::from_be_bytes(quad)
    })
}

fn has_exif_header(bytes: &[u8], offset: usize) -> bool {
    bytes.get(offset..offset + 6) == Some(b"Exif\0\0".as_slice())
}

fn orientation_from_tiff(bytes: &[u8], tiff_start: usize) -> Option<u16> {
    if tiff_start + 8 > bytes.len() {
        return None;
    }
    let byte_order = read_u16(bytes, tiff_start, false)?;
    let little_endian = byte_order == 0x4949;
    if !little_endian && byte_order != 0x4d4d {
        return None;
    }
    let ifd_offset = u64::from(read_u32(bytes, tiff_start + 4, little_endian)?);
    let ifd_start = usize::try_from(u64::try_from(tiff_start).ok()? + ifd_offset).ok()?;
    let entry_count = read_u16(bytes, ifd_start, little_endian)? as usize;
    for index in 0..entry_count {
        let entry = ifd_start
            .checked_add(2)?
            .checked_add(index.checked_mul(12)?)?;
        if entry + 12 > bytes.len() {
            return None;
        }
        if read_u16(bytes, entry, little_endian)? == 0x0112 {
            let value = read_u16(bytes, entry + 8, little_endian)?;
            return (1..=8).contains(&value).then_some(value);
        }
    }
    None
}

fn jpeg_tiff_offset(bytes: &[u8]) -> Option<usize> {
    let mut offset = 2usize;
    while offset + 1 < bytes.len() {
        if bytes[offset] != 0xff {
            return None;
        }
        let marker = bytes[offset + 1];
        if marker == 0xff {
            offset += 1;
            continue;
        }
        // Stop before entropy-coded data or the end of image; scanning past it is meaningless.
        if marker == 0xda || marker == 0xd9 {
            return None;
        }
        if marker == 0xe1 && offset + 4 < bytes.len() {
            let segment_start = offset + 4;
            if has_exif_header(bytes, segment_start) {
                return Some(segment_start + 6);
            }
            // An XMP APP1 segment before EXIF must be skipped, not treated as a terminator.
        }
        if offset + 4 > bytes.len() {
            return None;
        }
        let length = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        if length < 2 {
            return None;
        }
        offset = offset.checked_add(2)?.checked_add(length)?;
    }
    None
}

fn webp_tiff_offset(bytes: &[u8]) -> Option<usize> {
    let mut offset = 12usize;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().ok()?) as usize;
        let data_start = offset + 8;
        if id == b"EXIF" {
            if data_start + size > bytes.len() {
                return None;
            }
            return if size >= 6 && has_exif_header(bytes, data_start) {
                Some(data_start + 6)
            } else {
                Some(data_start)
            };
        }
        offset = data_start.checked_add(size)?.checked_add(size % 2)?;
    }
    None
}

/// Return the EXIF orientation tag (1..=8), or `None` when there is no valid tag.
pub fn parse_exif_orientation(bytes: &[u8]) -> Option<u16> {
    let tiff_start = if bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] == 0xd8 {
        jpeg_tiff_offset(bytes)?
    } else if bytes.len() >= 12 && bytes[0..4] == *b"RIFF" && bytes[8..12] == *b"WEBP" {
        webp_tiff_offset(bytes)?
    } else {
        return None;
    };
    orientation_from_tiff(bytes, tiff_start)
}

/// Apply orientations 5-8 by remapping pixels. Upstream's `rotate90` helper maps
/// `src(x,y) -> dst(h-1-y, x)` into an `h x w` image; the extra flip for 5 and 7 is folded into
/// the index formula so no intermediate allocation is needed.
fn remap_90(image: &DynamicImage, orientation: u16) -> DynamicImage {
    let source = image.to_rgba8();
    let (width, height) = source.dimensions();
    let mut output = RgbaImage::new(height, width);
    for y in 0..height {
        for x in 0..width {
            let (dx, dy) = match orientation {
                5 => (y, x),
                6 => (height - 1 - y, x),
                7 => (height - 1 - y, width - 1 - x),
                _ => (y, width - 1 - x), // orientation 8
            };
            output.put_pixel(dx, dy, *source.get_pixel(x, y));
        }
    }
    DynamicImage::ImageRgba8(output)
}

/// Apply EXIF orientation to a decoded image. Flips mutate upstream images in place; Rust's
/// `DynamicImage` methods return the transformed image instead.
pub fn apply_exif_orientation(image: DynamicImage, original_bytes: &[u8]) -> DynamicImage {
    let Some(orientation) = parse_exif_orientation(original_bytes) else {
        return image;
    };
    match orientation {
        1 => image,
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5..=8 => remap_90(&image, orientation),
        _ => image,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    fn tiff(little_endian: bool, orientation: u16) -> Vec<u8> {
        let mut out = Vec::new();
        if little_endian {
            out.extend_from_slice(b"II");
            out.extend_from_slice(&42u16.to_le_bytes());
            out.extend_from_slice(&8u32.to_le_bytes());
            out.extend_from_slice(&1u16.to_le_bytes());
            out.extend_from_slice(&0x0112u16.to_le_bytes());
            out.extend_from_slice(&3u16.to_le_bytes());
            out.extend_from_slice(&1u32.to_le_bytes());
            out.extend_from_slice(&orientation.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
        } else {
            out.extend_from_slice(b"MM");
            out.extend_from_slice(&42u16.to_be_bytes());
            out.extend_from_slice(&8u32.to_be_bytes());
            out.extend_from_slice(&1u16.to_be_bytes());
            out.extend_from_slice(&0x0112u16.to_be_bytes());
            out.extend_from_slice(&3u16.to_be_bytes());
            out.extend_from_slice(&1u32.to_be_bytes());
            out.extend_from_slice(&orientation.to_be_bytes());
            out.extend_from_slice(&0u16.to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes());
        }
        out
    }

    fn jpeg(exif: &[u8], xmp_first: bool) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8];
        if xmp_first {
            let xmp = b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta/>";
            out.extend_from_slice(&[0xff, 0xe1]);
            out.extend_from_slice(&((xmp.len() + 2) as u16).to_be_bytes());
            out.extend_from_slice(xmp);
        }
        out.extend_from_slice(&[0xff, 0xe1]);
        out.extend_from_slice(&((exif.len() + 8) as u16).to_be_bytes());
        out.extend_from_slice(b"Exif\0\0");
        out.extend_from_slice(exif);
        out.extend_from_slice(&[0xff, 0xd9]);
        out
    }

    fn webp(exif: &[u8], with_header: bool) -> Vec<u8> {
        let mut data = Vec::new();
        if with_header {
            data.extend_from_slice(b"Exif\0\0");
        }
        data.extend_from_slice(exif);
        let padded = data.len() + data.len() % 2;
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&((4 + 8 + padded) as u32).to_le_bytes());
        out.extend_from_slice(b"WEBP");
        out.extend_from_slice(b"EXIF");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        if padded != data.len() {
            out.push(0);
        }
        out
    }

    #[test]
    fn parses_little_endian_jpeg() {
        assert_eq!(
            parse_exif_orientation(&jpeg(&tiff(true, 6), false)),
            Some(6)
        );
    }

    #[test]
    fn parses_big_endian_jpeg() {
        assert_eq!(
            parse_exif_orientation(&jpeg(&tiff(false, 3), false)),
            Some(3)
        );
    }

    #[test]
    fn skips_xmp_app1_before_exif() {
        assert_eq!(parse_exif_orientation(&jpeg(&tiff(true, 6), true)), Some(6));
    }

    #[test]
    fn parses_webp_with_and_without_exif_header() {
        assert_eq!(parse_exif_orientation(&webp(&tiff(true, 8), true)), Some(8));
        assert_eq!(
            parse_exif_orientation(&webp(&tiff(false, 2), false)),
            Some(2)
        );
    }

    #[test]
    fn invalid_orientation_and_missing_exif_are_none() {
        assert_eq!(parse_exif_orientation(&jpeg(&tiff(true, 9), false)), None);
        assert_eq!(parse_exif_orientation(&jpeg(&tiff(true, 0), false)), None);
        assert_eq!(parse_exif_orientation(b"\xff\xd8\xff\xd9"), None);
        assert_eq!(parse_exif_orientation(&[]), None);
    }

    #[test]
    fn truncated_tiff_is_rejected() {
        let mut bytes = jpeg(&tiff(true, 6), false);
        bytes.truncate(bytes.len() - 10);
        assert_eq!(parse_exif_orientation(&bytes), None);
    }

    #[test]
    fn orientation_six_rotates_two_by_three_image() -> Result<(), String> {
        let mut image = image::RgbaImage::new(2, 3);
        for y in 0..3 {
            for x in 0..2 {
                image.put_pixel(x, y, image::Rgba([(x + 1) as u8, (y + 1) as u8, 0, 255]));
            }
        }
        let source = DynamicImage::ImageRgba8(image);
        let original = jpeg(&tiff(true, 6), false);
        let result = apply_exif_orientation(source, &original);
        assert_eq!(result.dimensions(), (3, 2));
        let pixels = result.to_rgba8();
        // src(0,0) -> dst(2,0), src(1,0) -> dst(2,1).
        assert_eq!(pixels.get_pixel(2, 0).0, [1, 1, 0, 255]);
        assert_eq!(pixels.get_pixel(2, 1).0, [2, 1, 0, 255]);
        Ok(())
    }

    #[test]
    fn orientation_five_and_seven_produce_transposed_dimensions() {
        let image = DynamicImage::new_rgba8(2, 3);
        for orientation in [5u16, 6, 7, 8] {
            let original = jpeg(&tiff(true, orientation), false);
            let result = apply_exif_orientation(image.clone(), &original);
            assert_eq!(result.dimensions(), (3, 2));
        }
    }

    #[test]
    fn identity_flips_keep_dimensions() {
        let image = DynamicImage::new_rgba8(2, 3);
        for orientation in [1u16, 2, 3, 4] {
            let original = jpeg(&tiff(true, orientation), false);
            assert_eq!(
                apply_exif_orientation(image.clone(), &original).dimensions(),
                (2, 3)
            );
        }
    }
}

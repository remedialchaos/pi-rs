//! Image signatures and dimensions without decoding pixel data.

use pi_rs_chord::context::Context;

use crate::{
    env::{BinaryReader, FileError},
    images::ImageSize,
};

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const HEADER_BYTES: u64 = 32;
const BLOCK_BYTES: u64 = 64 * 1024;

fn field<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    bytes.get(offset..offset.checked_add(N)?)?.try_into().ok()
}

fn is_png(header: &[u8]) -> bool {
    field(header, 8).map(u32::from_be_bytes) == Some(13) && header.get(12..16) == Some(b"IHDR")
}

fn is_bmp(header: &[u8]) -> bool {
    let Some((size, pixels, dib)) = field(header, 2)
        .zip(field(header, 10))
        .zip(field(header, 14))
        .map(|((size, pixels), dib)| {
            (
                u32::from_le_bytes(size),
                u32::from_le_bytes(pixels),
                u32::from_le_bytes(dib),
            )
        })
    else {
        return false;
    };
    if u64::from(pixels) < 14 + u64::from(dib) || (size != 0 && (size < 26 || pixels >= size)) {
        return false;
    }
    let (planes, depth) = match dib {
        12 => (22, 24),
        40..=124 => (26, 28),
        _ => return false,
    };
    field(header, planes).map(u16::from_le_bytes) == Some(1)
        && field(header, depth)
            .map(u16::from_le_bytes)
            .is_some_and(|n| matches!(n, 1 | 4 | 8 | 16 | 24 | 32))
}

enum PngStep {
    Animated,
    Done,
    Advance(u32),
}

fn png_step(header: &[u8]) -> PngStep {
    match header.get(4..8) {
        Some(b"acTL") => PngStep::Animated,
        Some(b"IDAT") | None => PngStep::Done,
        _ => field(header, 0).map_or(PngStep::Done, |length| {
            PngStep::Advance(u32::from_be_bytes(length))
        }),
    }
}

fn is_animated_png(bytes: &[u8]) -> bool {
    let mut position = PNG_SIGNATURE.len();
    while let Some(header) = bytes.get(position..) {
        match png_step(header) {
            PngStep::Animated => return true,
            PngStep::Done => return false,
            PngStep::Advance(length) => {
                let Some(next) = position
                    .checked_add(12)
                    .and_then(|pos| pos.checked_add(length as usize))
                else {
                    return false;
                };
                position = next;
            }
        }
    }
    false
}

pub fn detect_supported_image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\xff\xd8\xff") {
        return (bytes.get(3) != Some(&0xf7)).then_some("image/jpeg");
    }
    if bytes.starts_with(PNG_SIGNATURE) {
        return (is_png(bytes) && !is_animated_png(bytes)).then_some("image/png");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    if bytes.starts_with(b"BM") && is_bmp(bytes) {
        return Some("image/bmp");
    }
    None
}

/// Read only a file's header and, for PNG, chunks up to the first animation control or image-data chunk.
pub async fn detect_supported_image_mime_type_of(
    reader: &mut dyn BinaryReader,
    size: u64,
    context: &Context,
) -> Result<Option<&'static str>, FileError> {
    let header = reader.read(0, HEADER_BYTES, context).await?;
    if !header.starts_with(PNG_SIGNATURE) {
        return Ok(detect_supported_image_mime_type(&header));
    }
    if !is_png(&header) {
        return Ok(None);
    }
    let mut position = PNG_SIGNATURE.len() as u64;
    let mut block_start = 0;
    let mut block = header;
    while position <= size.saturating_sub(8) {
        let mut index = position - block_start;
        if index.saturating_add(8) > block.len() as u64 {
            block = reader.read(position, BLOCK_BYTES, context).await?;
            block_start = position;
            index = 0;
        }
        match png_step(&block[index as usize..]) {
            PngStep::Animated => return Ok(None),
            PngStep::Done => break,
            PngStep::Advance(length) => position = position.saturating_add(12 + u64::from(length)),
        }
    }
    Ok(Some("image/png"))
}

fn jpeg_dimensions(mut bytes: &[u8]) -> Option<ImageSize> {
    bytes = bytes.get(2..)?;
    while bytes.len() >= 9 && bytes[0] == 0xff {
        let marker = bytes[1];
        if marker == 0xff {
            bytes = &bytes[1..];
            continue;
        }
        if matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            return Some(ImageSize {
                width: u32::from(u16::from_be_bytes(field(bytes, 7)?)),
                height: u32::from(u16::from_be_bytes(field(bytes, 5)?)),
            });
        }
        let length = u16::from_be_bytes(field(bytes, 2)?);
        bytes = bytes.get(2 + usize::from(length)..)?;
    }
    None
}

pub fn image_dimensions(bytes: &[u8], mime_type: &str) -> Option<ImageSize> {
    let (width, height) = match mime_type {
        "image/png" => (
            u32::from_be_bytes(field(bytes, 16)?),
            u32::from_be_bytes(field(bytes, 20)?),
        ),
        "image/gif" => (
            u32::from(u16::from_le_bytes(field(bytes, 6)?)),
            u32::from(u16::from_le_bytes(field(bytes, 8)?)),
        ),
        "image/webp" if bytes.len() >= 30 => match bytes.get(12..16)? {
            b"VP8 " => (
                u32::from(u16::from_le_bytes(field(bytes, 26)?) & 0x3fff),
                u32::from(u16::from_le_bytes(field(bytes, 28)?) & 0x3fff),
            ),
            b"VP8L" => {
                let bits = u32::from_le_bytes(field(bytes, 21)?);
                ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
            }
            b"VP8X" => {
                let read = |at| {
                    field::<3>(bytes, at).map(|[a, b, c]| u32::from_le_bytes([a, b, c, 0]) + 1)
                };
                (read(24)?, read(27)?)
            }
            _ => return None,
        },
        "image/jpeg" => return jpeg_dimensions(bytes),
        _ => return None,
    };
    Some(ImageSize { width, height })
}

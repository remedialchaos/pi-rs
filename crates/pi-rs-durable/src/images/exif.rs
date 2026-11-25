const EXIF_PREFIX: &[u8] = b"Exif\0\0";

fn jpeg_exif(bytes: &[u8]) -> Option<&[u8]> {
    let mut remaining = bytes.get(2..)?;
    while remaining.len() >= 4 && remaining[0] == 0xff {
        if remaining[1] == 0xff {
            remaining = &remaining[1..];
            continue;
        }
        let length = usize::from(u16::from_be_bytes([remaining[2], remaining[3]]));
        let end = 2 + length;
        if remaining[1] == 0xe1
            && let Some(exif) = remaining.get(4..end)?.strip_prefix(EXIF_PREFIX)
        {
            return Some(exif);
        }
        remaining = remaining.get(end..)?;
    }
    None
}

fn webp_exif(bytes: &[u8]) -> Option<&[u8]> {
    let mut remaining = bytes.get(12..)?;
    while remaining.len() >= 8 {
        let size = usize::try_from(u32::from_le_bytes(remaining[4..8].try_into().ok()?)).ok()?;
        let end = 8usize.checked_add(size)?;
        if remaining.starts_with(b"EXIF") {
            let exif = remaining.get(8..end)?;
            return Some(exif.strip_prefix(EXIF_PREFIX).unwrap_or(exif));
        }
        remaining = remaining.get(end.checked_add(size % 2)?..)?;
    }
    None
}

fn tiff_orientation(bytes: &[u8]) -> Option<u8> {
    let little = match bytes.get(..4)? {
        b"II\x2a\0" => true,
        b"MM\0\x2a" => false,
        _ => return None,
    };
    let read16 = if little {
        u16::from_le_bytes
    } else {
        u16::from_be_bytes
    };
    let read32 = if little {
        u32::from_le_bytes
    } else {
        u32::from_be_bytes
    };
    let directory = usize::try_from(read32(bytes.get(4..8)?.try_into().ok()?)).ok()?;
    let entries = bytes.get(directory..)?;
    let count = usize::from(read16(entries.get(..2)?.try_into().ok()?));
    for entry in entries.get(2..)?.as_chunks::<12>().0.iter().take(count) {
        if read16(entry[..2].try_into().ok()?) == 0x0112 {
            let orientation = read16(entry[8..10].try_into().ok()?);
            return (1..=8).contains(&orientation).then_some(orientation as u8);
        }
    }
    None
}

/// EXIF orientation of a JPEG or WebP, or 1 if absent, invalid or unreadable.
pub fn exif_orientation(bytes: &[u8]) -> u8 {
    let exif = if bytes.starts_with(b"\xff\xd8") {
        jpeg_exif(bytes)
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        webp_exif(bytes)
    } else {
        None
    };
    exif.and_then(tiff_orientation).unwrap_or(1)
}

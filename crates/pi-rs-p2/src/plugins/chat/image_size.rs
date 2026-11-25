use pi_rs_durable::images::{exif_orientation, from_base64};

pub(super) fn size(data: &str) -> Option<(u32, u32)> {
    let header = data.get(..4096).unwrap_or(data);
    let bytes = from_base64(header).ok()?;
    let size = match bytes.as_slice() {
        [0xff, 0xd8, ..] => jpeg(&bytes).or_else(|| {
            (header.len() != data.len())
                .then(|| from_base64(data).ok().and_then(|bytes| jpeg(&bytes)))
                .flatten()
        }),
        [0x89, b'P', b'N', b'G', ..] if bytes.get(12..16)? == b"IHDR" => Some((
            u32::from_be_bytes(array(&bytes, 16)?),
            u32::from_be_bytes(array(&bytes, 20)?),
        )),
        [b'G', b'I', b'F', ..] => Some((
            u32::from(u16::from_le_bytes(array(&bytes, 6)?)),
            u32::from(u16::from_le_bytes(array(&bytes, 8)?)),
        )),
        [b'R', b'I', b'F', b'F', ..] if bytes.get(8..12)? == b"WEBP" => match bytes.get(12..16)? {
            b"VP8 " => Some((
                u32::from(u16::from_le_bytes(array(&bytes, 26)?) & 0x3fff),
                u32::from(u16::from_le_bytes(array(&bytes, 28)?) & 0x3fff),
            )),
            b"VP8L" => {
                let bits = u32::from_le_bytes(array(&bytes, 21)?);
                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            b"VP8X" => {
                let width = u32::from_le_bytes(array(&bytes, 24)?) & 0xffffff;
                let height = u32::from_le_bytes(array(&bytes, 27)?) & 0xffffff;
                Some((width + 1, height + 1))
            }
            _ => None,
        },
        _ => None,
    };
    size.filter(|(width, height)| *width > 0 && *height > 0)
}

fn jpeg(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut offset = 2;
    while bytes.get(offset) == Some(&0xff) {
        let marker = *bytes.get(offset + 1)?;
        match marker {
            0xff => offset += 1,
            0x01 | 0xd0..=0xd8 => offset += 2,
            0xda | 0xd9 => return None,
            0xc0..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                let height = u32::from(u16::from_be_bytes(array(bytes, offset + 5)?));
                let width = u32::from(u16::from_be_bytes(array(bytes, offset + 7)?));
                return Some(if exif_orientation(bytes) >= 5 {
                    (height, width)
                } else {
                    (width, height)
                });
            }
            _ => {
                let length = usize::from(u16::from_be_bytes(array(bytes, offset + 2)?));
                if length < 2 {
                    return None;
                }
                offset += length + 2;
            }
        }
    }
    None
}

fn array<const N: usize>(bytes: &[u8], index: usize) -> Option<[u8; N]> {
    bytes.get(index..index + N)?.try_into().ok()
}

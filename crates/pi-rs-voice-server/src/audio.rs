use crate::{Error, Result};

pub fn pcm16(audio: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(audio.len() * 2);
    append_pcm16(&mut bytes, audio);
    bytes
}

pub(crate) fn append_pcm16(bytes: &mut Vec<u8>, audio: &[f32]) {
    bytes.extend(
        audio
            .iter()
            .flat_map(|&sample| ((sample.clamp(-1.0, 1.0) * 32_767.0) as i16).to_le_bytes()),
    );
}

pub fn pcm16_to_float(pcm: &[u8]) -> Result<Vec<f32>> {
    let (samples, remainder) = pcm.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(Error::Request("expected PCM16LE samples".into()));
    }
    Ok(samples
        .iter()
        .map(|&sample| f32::from(i16::from_le_bytes(sample)) / 32_768.0)
        .collect())
}

pub fn wav(mut bytes: Vec<u8>, rate: u32) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(44 + bytes.len());
    out.extend_from_slice(&riff_header(rate, bytes.len())?);
    out.append(&mut bytes);
    Ok(out)
}

pub(crate) fn riff_header(rate: u32, len: usize) -> Result<[u8; 44]> {
    let len = u32::try_from(len)
        .ok()
        .filter(|&n| n <= u32::MAX - 36)
        .ok_or_else(|| Error::Model("WAV data exceeds RIFF limit".into()))?;
    let byte_rate = rate
        .checked_mul(2)
        .ok_or_else(|| Error::Model("invalid WAV sample rate".into()))?;
    let mut header =
        *b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\0\0\0\0\0\0\0\0\x02\0\x10\0data\0\0\0\0";
    header[4..8].copy_from_slice(&(len + 36).to_le_bytes());
    header[24..28].copy_from_slice(&rate.to_le_bytes());
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[40..44].copy_from_slice(&len.to_le_bytes());
    Ok(header)
}

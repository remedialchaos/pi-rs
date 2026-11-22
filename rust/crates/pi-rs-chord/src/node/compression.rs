//! Raw DEFLATE level 1 and bounded, complete-stream inflate.
use flate2::{Compression, Decompress, FlushDecompress, Status, write::DeflateEncoder};
use std::io::Write;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inflated {
    Bytes(Vec<u8>),
    TooLarge,
    Corrupt,
}

pub fn deflate(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(1));
    encoder.write_all(bytes)?;
    encoder.finish()
}

pub fn inflate(bytes: &[u8], limit: usize) -> Inflated {
    let mut decoder = Decompress::new(false);
    let mut output = Vec::new();
    loop {
        let mut piece = [0u8; 8192];
        let room = limit
            .saturating_sub(output.len())
            .saturating_add(1)
            .min(piece.len());
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let result = decoder.decompress(
            &bytes[before_in as usize..],
            &mut piece[..room],
            FlushDecompress::Finish,
        );
        let written = (decoder.total_out() - before_out) as usize;
        output.extend_from_slice(&piece[..written]);
        if output.len() > limit {
            return Inflated::TooLarge;
        }
        match result {
            Ok(Status::StreamEnd) => return Inflated::Bytes(output),
            Err(_) => return Inflated::Corrupt,
            Ok(_) if decoder.total_in() == before_in && written == 0 => return Inflated::Corrupt,
            Ok(_) => {}
        }
    }
}

//! Standalone native worker peer used by the real subprocess acceptance test.
use std::io::{Read, Write};
fn main() -> std::io::Result<()> {
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let marker = std::env::var("STARTS").map_err(std::io::Error::other)?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(marker)?
        .write_all(b"1")?;
    let start = read(&mut input)?;
    if String::from_utf8_lossy(&start).contains("crash") {
        return Ok(());
    }
    if !String::from_utf8_lossy(&start).contains("plug") {
        return Err(std::io::Error::other("plugin not delivered"));
    }
    send(
        &mut output,
        0,
        br#"{"chord.workers":{"kind":"ready","idle":true}}"#,
    )?;
    loop {
        let frame = read(&mut input)?;
        if frame.first() == Some(&1) {
            send(&mut output, 1, &frame[1..])?;
        } else if String::from_utf8_lossy(&frame).contains("shutdown") {
            return Ok(());
        }
    }
}
fn read(input: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut length = [0; 4];
    input.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > 17 * 1024 * 1024 {
        return Err(std::io::Error::other("too large"));
    }
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}
fn send(output: &mut impl Write, tag: u8, body: &[u8]) -> std::io::Result<()> {
    output.write_all(&((body.len() + 1) as u32).to_be_bytes())?;
    output.write_all(&[tag])?;
    output.write_all(body)?;
    output.flush()
}

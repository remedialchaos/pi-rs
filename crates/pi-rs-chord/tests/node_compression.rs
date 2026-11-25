use pi_rs_chord::node::compression::{Inflated, deflate, inflate};

#[test]
fn raw_deflate_roundtrip_and_exact_inflate_bounds() {
    let bytes = b"hello hello hello";
    let compressed = deflate(bytes).unwrap();
    assert_eq!(
        inflate(&compressed, bytes.len()),
        Inflated::Bytes(bytes.to_vec())
    );
    assert_eq!(inflate(&compressed, bytes.len() - 1), Inflated::TooLarge);
    assert_eq!(inflate(&compressed, 0), Inflated::TooLarge);
    assert_eq!(inflate(&deflate(b"").unwrap(), 0), Inflated::Bytes(vec![]));
}

#[test]
fn raw_inflate_rejects_corrupt_and_truncated_streams() {
    assert_eq!(inflate(&[255, 255, 255], 1024), Inflated::Corrupt);
    let mut bytes = deflate(b"some repeated repeated bytes").unwrap();
    bytes.truncate(bytes.len() / 2);
    assert_eq!(inflate(&bytes, 1024), Inflated::Corrupt);
}

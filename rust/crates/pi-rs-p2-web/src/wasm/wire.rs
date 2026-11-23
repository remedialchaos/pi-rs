use crate::chord_bridge::{decode_varint, encode_part, encode_string, encode_varint};
use serde_json::{Value, json};

pub type Result<T> = std::result::Result<T, String>;

pub fn hold(id: u64, session: Option<(&str, u64)>) -> Vec<u8> {
    // Version, fingerprint and sorted member indexes from p2's declared contracts.
    let (name, version, hash, params, address, route) = match session {
        Some((session, conversation)) => (
            "p2.chat",
            6,
            0x15b5fd3b4b340ecau64,
            json!({ "session":session,"conversation":conversation}),
            "session",
            json!({"id":session}),
        ),
        None if id == 1 => (
            "p2.access",
            1,
            0x2435548bc2ca4912,
            json!({}),
            "server",
            json!({}),
        ),
        None => (
            "p2.sessions",
            7,
            0x1e273ec42b804553,
            json!({}),
            "server",
            json!({}),
        ),
    };
    let mut buf = vec![1];
    encode_varint(id, &mut buf);
    encode_string(name, &mut buf);
    encode_varint(version, &mut buf);
    buf.extend(hash.to_be_bytes());
    buf.extend(encode_part(params.to_string().as_bytes(), false));
    encode_string(address, &mut buf);
    encode_string(&route.to_string(), &mut buf);
    buf.push(0);
    buf
}
pub fn call(id: u64, hold: u64, method: u64, args: Value) -> Vec<u8> {
    let mut buf = vec![3];
    for field in [id, hold, method, 1] {
        encode_varint(field, &mut buf);
    }
    buf.extend(encode_part(args.to_string().as_bytes(), false));
    buf
}
pub struct Reader<'a>(pub &'a [u8]);
impl<'a> Reader<'a> {
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n).ok_or("truncated Chord frame")?;
        self.0 = tail;
        Ok(head)
    }
    pub fn number(&mut self) -> Result<u64> {
        let (v, n) = decode_varint(self.0).map_err(|_| "invalid Chord integer")?;
        self.take(n)?;
        Ok(v)
    }
    pub fn json(&mut self) -> Result<Value> {
        let size = usize::try_from(self.number()?).map_err(|_| "oversized JSON")?;
        serde_json::from_slice(self.take(size)?).map_err(|e| e.to_string())
    }
    pub fn part(&mut self) -> Result<Value> {
        let flags = self.number()?;
        let size = usize::try_from(flags / 2).map_err(|_| "oversized part")?;
        if size > 32 * 1024 * 1024 {
            return Err("Chord part exceeds 32 MiB".into());
        }
        let bytes = self.take(size)?;
        let plain;
        let bytes = if flags & 1 == 1 {
            plain = miniz_oxide::inflate::decompress_to_vec_with_limit(bytes, 128 * 1024 * 1024)
                .map_err(|_| "invalid compressed Chord part")?;
            &plain[..]
        } else {
            bytes
        };
        serde_json::from_slice(bytes).map_err(|e| e.to_string())
    }
}

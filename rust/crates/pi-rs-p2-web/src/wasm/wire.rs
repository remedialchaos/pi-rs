use crate::browser_plugins::{
    P2_ACCESS, P2_CHAT, P2_SESSIONS, encode_call_frame, hold_for_contract,
};
use crate::chord_bridge::decode_varint;
use serde_json::Value;

pub type Result<T> = std::result::Result<T, String>;

pub fn hold(id: u64, session: Option<(&str, u64)>) -> Vec<u8> {
    match session {
        Some((session, conversation)) => {
            hold_for_contract(id, &P2_CHAT, Some(session), Some(conversation), None)
        }
        None if id == 1 => hold_for_contract(id, &P2_ACCESS, None, None, None),
        None => hold_for_contract(id, &P2_SESSIONS, None, None, None),
    }
}
pub fn call(id: u64, hold: u64, method: u64, args: Value) -> Vec<u8> {
    encode_call_frame(id, hold, method, &args)
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

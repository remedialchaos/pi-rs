//! Wire format for the local owner challenge, used by the devices facet and local clients.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::fmt::Write;

pub const LOCAL_CHALLENGE_PATH: &str = "/owner/local/challenge";
pub const LOCAL_CLAIM_PATH: &str = "/owner/local/claim";

pub fn local_owner_proof(key: &str, port: u16, challenge: &str) -> String {
    // INFALLIBLE: HMAC accepts keys of any length.
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key");
    mac.update(format!("p2-local-owner:{port}:{challenge}").as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

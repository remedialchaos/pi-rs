//! The Radius relay's access: how a request through the relay is told apart from a local one.
//!
//! The relay client (the brain) serves relayed browsers by making loopback requests to this
//! server, which would otherwise look like the device itself and receive the localhost-only
//! owner powers. It marks them with a per-process secret: a marked request comes from a browser
//! Radius authenticated on the cortex origin, and is treated as remote, so every localhost-only
//! path stays closed.

use base64::Engine;
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Marks loopback requests the Radius relay client makes on behalf of a relayed browser.
pub const RELAY_SECRET_HEADER: &str = "x-p2-relay";
/// The brain's browser-facing origin (its cortex origin), e.g. `https://<label>.rdtun.com`.
pub const RELAY_ORIGIN_HEADER: &str = "x-p2-relay-origin";
/// The Radius shell origin that frames the brain UI; absent for a Pi client of the owner, which no page frames.
pub const RELAY_SHELL_HEADER: &str = "x-p2-relay-shell";
/// A Radius user other than the owner: base64url JSON `{ id, email }`. Absent for the owner.
pub const RELAY_USER_HEADER: &str = "x-p2-relay-user";

/// A Radius user other than the owner.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RelayUser {
    pub id: String,
    pub email: String,
}

/// A request that came through the relay.
#[derive(Clone, Debug, PartialEq)]
pub struct RelayedRequest {
    pub public_origin: String,
    pub shell_origin: Option<String>,
    pub user: Option<RelayUser>,
}

/// The relay's access: the secret that marks its loopback requests.
#[derive(Clone)]
pub struct RelayAccess {
    secret: String,
}

impl Default for RelayAccess {
    fn default() -> Self {
        Self::new()
    }
}

impl RelayAccess {
    /// A new secret for this process.
    pub fn new() -> Self {
        let mut bytes = [0u8; 32];
        bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
        Self {
            secret: hex(&bytes),
        }
    }

    /// The secret this process marks its relayed requests with.
    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// Distinguishes a relayed request from a local one.
    ///
    /// Without the secret the marker headers mean nothing, so neither browsers nor other local
    /// processes can claim them. Without the shell header it is the owner's Pi client, the TUI
    /// launcher through Radius; with it, it is a person the owner shared sessions with by email.
    pub fn verify(&self, local_address: &str, headers: &HeaderMap) -> Option<RelayedRequest> {
        if local_address != "127.0.0.1" {
            return None;
        }
        let presented = headers.get(RELAY_SECRET_HEADER)?.to_str().ok()?;
        if !constant_time_eq(presented.as_bytes(), self.secret.as_bytes()) {
            return None;
        }
        let public_origin = headers.get(RELAY_ORIGIN_HEADER)?.to_str().ok()?;
        if !is_origin(public_origin) {
            return None;
        }
        let shell = headers.get(RELAY_SHELL_HEADER);
        let user = headers.get(RELAY_USER_HEADER);
        let Some(shell_origin) = shell else {
            // Clients are the owner's only; sharing admits people on the brain's UI.
            return match user {
                None => Some(RelayedRequest {
                    public_origin: public_origin.to_owned(),
                    shell_origin: None,
                    user: None,
                }),
                Some(_) => None,
            };
        };
        let shell_origin = shell_origin.to_str().ok()?;
        if !is_origin(shell_origin) {
            return None;
        }
        match user {
            None => Some(RelayedRequest {
                public_origin: public_origin.to_owned(),
                shell_origin: Some(shell_origin.to_owned()),
                user: None,
            }),
            Some(user) => {
                let user = user.to_str().ok()?;
                Some(RelayedRequest {
                    public_origin: public_origin.to_owned(),
                    shell_origin: Some(shell_origin.to_owned()),
                    user: Some(decode_relay_user(user)?),
                })
            }
        }
    }
}

/// The header value of a Radius user; the brain writes it, so it is plain ASCII whatever the name.
pub fn encode_relay_user(user: &RelayUser) -> String {
    let value = serde_json::json!({"id": user.id, "email": user.email});
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
}

/// The user a header carries.
fn decode_relay_user(value: &str) -> Option<RelayUser> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .ok()?;
    let text = String::from_utf8(bytes).ok()?;
    serde_json::from_str(&text).ok()
}

/// An origin: a scheme, a host and nothing else.
fn is_origin(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "http" | "https") {
        return false;
    }
    !rest.is_empty() && !rest.contains('/') && !rest.chars().any(char::is_whitespace)
}

/// The bytes of `value` in lower-case hexadecimal.
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = std::fmt::Write::write_fmt(&mut text, format_args!("{byte:02x}"));
    }
    text
}

/// Whether two byte strings are the same, without leaking where they differ.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in left.iter().zip(right.iter()) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    /// The headers of a relayed request.
    fn relay_headers(
        access: &RelayAccess,
        origin: &str,
        shell: Option<&str>,
        user: Option<&str>,
    ) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            RELAY_SECRET_HEADER,
            HeaderValue::from_str(access.secret()).expect("a hex secret"),
        );
        headers.insert(
            RELAY_ORIGIN_HEADER,
            HeaderValue::from_str(origin).expect("an origin"),
        );
        if let Some(shell) = shell {
            headers.insert(
                RELAY_SHELL_HEADER,
                HeaderValue::from_str(shell).expect("an origin"),
            );
        }
        if let Some(user) = user {
            headers.insert(
                RELAY_USER_HEADER,
                HeaderValue::from_str(user).expect("a user"),
            );
        }
        headers
    }

    #[test]
    fn a_relayed_request_carries_its_origins() {
        let access = RelayAccess::new();
        let headers = relay_headers(&access, "https://brain.rdtun.com", None, None);
        let relayed = access
            .verify("127.0.0.1", &headers)
            .expect("a relayed request");
        assert_eq!(relayed.public_origin, "https://brain.rdtun.com");
        assert_eq!(relayed.shell_origin, None);
        assert_eq!(relayed.user, None);
    }

    #[test]
    fn without_the_secret_the_markers_mean_nothing() {
        let access = RelayAccess::new();
        let mut headers = relay_headers(&access, "https://brain.rdtun.com", None, None);
        headers.insert(RELAY_SECRET_HEADER, HeaderValue::from_static("wrong"));
        assert!(access.verify("127.0.0.1", &headers).is_none());
        let mut headers = relay_headers(&access, "https://brain.rdtun.com", None, None);
        headers.remove(RELAY_SECRET_HEADER);
        assert!(access.verify("127.0.0.1", &headers).is_none());
    }

    #[test]
    fn a_request_that_is_not_loopback_is_not_relayed() {
        let access = RelayAccess::new();
        let headers = relay_headers(&access, "https://brain.rdtun.com", None, None);
        assert!(access.verify("10.0.0.1", &headers).is_none());
    }

    #[test]
    fn an_origin_that_is_not_an_origin_is_refused() {
        let access = RelayAccess::new();
        for origin in [
            "brain.rdtun.com",
            "https://brain.rdtun.com/path",
            "ftp://brain",
        ] {
            let headers = relay_headers(&access, origin, None, None);
            assert!(access.verify("127.0.0.1", &headers).is_none(), "{origin}");
        }
    }

    #[test]
    fn a_person_the_owner_shared_with_needs_the_shell_origin() {
        let access = RelayAccess::new();
        let user = RelayUser {
            id: "u1".into(),
            email: "person@example.com".into(),
        };
        let encoded = encode_relay_user(&user);
        let headers = relay_headers(&access, "https://brain.rdtun.com", None, Some(&encoded));
        assert!(access.verify("127.0.0.1", &headers).is_none());
        let headers = relay_headers(
            &access,
            "https://brain.rdtun.com",
            Some("https://radius.example"),
            Some(&encoded),
        );
        let relayed = access
            .verify("127.0.0.1", &headers)
            .expect("a relayed request");
        assert_eq!(
            relayed.shell_origin.as_deref(),
            Some("https://radius.example")
        );
        assert_eq!(relayed.user, Some(user));
    }

    #[test]
    fn a_user_that_is_not_a_user_is_refused() {
        let access = RelayAccess::new();
        let headers = relay_headers(
            &access,
            "https://brain.rdtun.com",
            Some("https://radius.example"),
            Some("not base64url json"),
        );
        assert!(access.verify("127.0.0.1", &headers).is_none());
    }
}

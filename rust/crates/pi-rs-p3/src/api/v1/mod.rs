//! p3's API, version 1, for every host kind: the base types a plugin may use, the host kinds, the
//! helpers for plugins (`forward`, `contribute`), and the control messages between the server and a
//! session's worker.
//!
//! Upstream's module is `@earendil-works/p3/v1`; this port's is `pi_rs_p3::api::v1`. Plugins are
//! compiled in (ADR-002), so the module system that once kept a worker from importing the server's
//! HTTP stack is the module boundary itself.

pub mod contribute;
pub mod control;
pub mod forward;

use serde_json::{Value, json};

use crate::principals::Principal;

pub use crate::principals::Principal as P3Principal;

/// The address of the server: one per Pi.
pub fn server_address() -> Value {
    json!({ "kind": "server" })
}

/// The address of session `id`: a worker process of its own, with its own store.
pub fn session_address(id: &str) -> Value {
    json!({ "kind": "session", "id": id })
}

/// The session an address names, or `None` for an address of another kind.
pub fn session_of(address: &Value) -> Option<&str> {
    if address.get("kind")?.as_str()? != "session" {
        return None;
    }
    address.get("id")?.as_str()
}

/// A principal as JSON: what an address, a link, or a sign-in carries.
pub fn principal_value(principal: &Principal) -> Value {
    principal.value()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_host_kinds_have_their_addresses() {
        assert_eq!(server_address(), json!({ "kind": "server" }));
        assert_eq!(
            session_address("s1"),
            json!({ "kind": "session", "id": "s1" })
        );
        assert_eq!(session_of(&session_address("s1")), Some("s1"));
        assert_eq!(session_of(&server_address()), None);
        assert_eq!(session_of(&json!({"kind":"session"})), None);
    }

    #[test]
    fn a_principal_reaches_json_as_it_is() {
        assert_eq!(
            principal_value(&Principal::worker("s1")),
            json!({ "kind": "worker", "session": "s1" })
        );
    }
}

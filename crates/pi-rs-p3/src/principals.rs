//! p3's principals (decision "The principal is identity only", 2026-11-13): who someone is, never
//! what they may do. What a person may do is data of apps and plugins, read live by admissions.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Who someone is in p3: the owner, a person the owner let in, a session's worker, or the server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Principal {
    Owner,
    Person { id: String, name: String },
    Worker { session: String },
    Server,
}

impl Principal {
    /// The principal as the JSON p3 stores and sends.
    pub fn value(&self) -> Value {
        let mut object = Map::new();
        match self {
            Self::Owner => {
                object.insert("kind".into(), Value::String("owner".into()));
            }
            Self::Person { id, name } => {
                object.insert("kind".into(), Value::String("person".into()));
                object.insert("id".into(), Value::String(id.clone()));
                object.insert("name".into(), Value::String(name.clone()));
            }
            Self::Worker { session } => {
                object.insert("kind".into(), Value::String("worker".into()));
                object.insert("session".into(), Value::String(session.clone()));
            }
            Self::Server => {
                object.insert("kind".into(), Value::String("server".into()));
            }
        }
        Value::Object(object)
    }

    /// The principal a JSON value names, or `None` for a value that is no principal.
    pub fn from_value(value: &Value) -> Option<Self> {
        // A principal is an object tagged `kind`: serde's tagged-enum decoder would otherwise read a
        // sequence as a unit variant (`["owner"]` as the owner).
        if !value.is_object() {
            return None;
        }
        serde_json::from_value(value.clone()).ok()
    }

    /// The principal of session `id`'s worker.
    pub fn worker(session: impl Into<String>) -> Self {
        Self::Worker {
            session: session.into(),
        }
    }

    /// Whether the principal is the owner.
    pub fn is_owner(&self) -> bool {
        matches!(self, Self::Owner)
    }

    /// The session a worker principal names.
    pub fn session(&self) -> Option<&str> {
        match self {
            Self::Worker { session } => Some(session),
            _ => None,
        }
    }
}

/// The server's principal.
pub const SERVER: Principal = Principal::Server;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principals_round_trip_through_their_json_shape() {
        let cases = [
            (Principal::Owner, r#"{"kind":"owner"}"#),
            (
                Principal::Person {
                    id: "p1".into(),
                    name: "Ann".into(),
                },
                r#"{"kind":"person","id":"p1","name":"Ann"}"#,
            ),
            (
                Principal::worker("s1"),
                r#"{"kind":"worker","session":"s1"}"#,
            ),
            (Principal::Server, r#"{"kind":"server"}"#),
        ];
        for (principal, json) in cases {
            let value: Value = serde_json::from_str(json).expect("test JSON parses");
            assert_eq!(principal.value(), value);
            assert_eq!(Principal::from_value(&value), Some(principal));
        }
    }

    #[test]
    fn an_unknown_kind_or_a_missing_field_is_no_principal() {
        for json in [
            r#"{"kind":"admin"}"#,
            r#"{"kind":"person","id":"p1"}"#,
            r#"{"kind":"worker"}"#,
            "null",
            r#"["owner"]"#,
        ] {
            let value: Value = serde_json::from_str(json).expect("test JSON parses");
            assert_eq!(Principal::from_value(&value), None, "{json}");
        }
    }
}

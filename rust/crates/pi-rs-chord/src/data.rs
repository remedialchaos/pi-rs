//! Chord errors, principal identity and compiled schema checks.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub mod args;

pub const PROTOCOL_VERSION: u32 = 1;
pub const ERROR_CODES: [&str; 9] = [
    "invalid",
    "contract_mismatch",
    "forbidden",
    "gone",
    "not_ready",
    "released",
    "too_large",
    "unavailable",
    "aborted",
];
pub fn error_reasons(code: &str) -> Option<&'static [&'static str]> {
    Some(match code {
        "invalid" => &[
            "args",
            "params",
            "member",
            "host_address",
            "reentrant",
            "read_after_bulk",
            "cycle",
        ],
        "contract_mismatch" => &["updating"],
        "forbidden" => &["revoked"],
        "gone" | "not_ready" | "aborted" => &[],
        "released" => &["ended", "closed", "settled", "version", "discarded"],
        "too_large" => &["part"],
        "unavailable" => &[
            "locked",
            "park_timeout",
            "busy",
            "storage_failed",
            "undeclared",
            "version",
            "reload_cycle",
        ],
        _ => return None,
    })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
#[serde(deny_unknown_fields)]
pub struct ChordError {
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}
impl ChordError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            reason: None,
            message: message.into(),
            details: None,
        }
    }
    pub fn invalid(
        reason: impl Into<String>,
        message: impl Into<String>,
        details: Option<Value>,
    ) -> Self {
        Self {
            code: "invalid".into(),
            reason: Some(reason.into()),
            message: message.into(),
            details,
        }
    }
    pub fn from_json(value: Value) -> Result<Self> {
        let invalid =
            |problem: String| Error::Invalid(format!("Invalid ChordError JSON: {problem}"));
        let object = value
            .as_object()
            .ok_or_else(|| invalid("not an object".into()))?;
        for key in object.keys() {
            if !["code", "reason", "message", "details"].contains(&key.as_str()) {
                return Err(invalid(format!("unknown key {key:?}")));
            }
        }
        let code = object
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("unknown code".into()))?;
        let reasons = error_reasons(code).ok_or_else(|| invalid("unknown code".into()))?;
        let reason = object.get("reason");
        if reason.map_or(code == "invalid", |r| {
            r.as_str().is_none_or(|r| !reasons.contains(&r))
        }) {
            return Err(invalid(format!("reason not allowed for {code}")));
        }
        if !object.get("message").is_some_and(Value::is_string) {
            return Err(invalid("message is not a string".into()));
        }
        Ok(serde_json::from_value(value)?)
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct PluginError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailureKind {
    Transient,
    Broken,
    Waiting,
    Denied,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FailureCause {
    pub kind: FailureKind,
    pub error: ChordError,
}

pub fn waiting_for(blocker: &str, error: &ChordError) -> FailureCause {
    let mut root = error.message.as_str();
    while let Some(after) = root.strip_prefix("waiting for ") {
        if let Some((_, rest)) = after.split_once(": ") {
            root = rest;
        } else {
            break;
        }
    }
    let mut root: Vec<u16> = root.encode_utf16().collect();
    let mut label: Vec<u16> = blocker.encode_utf16().collect();
    let room = 500 - "waiting for ".len() - ": ".len();
    if label.len() + root.len() > room {
        let root_room = 320.max(room.saturating_sub(label.len()));
        if root.len() > root_room {
            root.truncate(root_room - 1);
            root.push(0x2026);
        }
        if label.len() + root.len() > room {
            label.truncate(room - root.len() - 1);
            label.push(0x2026);
        }
    }
    let version = error.code == "contract_mismatch"
        || (error.code == "unavailable" && error.reason.as_deref() == Some("version"));
    FailureCause {
        kind: FailureKind::Waiting,
        error: ChordError {
            code: "unavailable".into(),
            reason: version.then(|| "version".into()),
            message: format!(
                "waiting for {}: {}",
                String::from_utf16_lossy(&label),
                String::from_utf16_lossy(&root)
            ),
            details: None,
        },
    }
}
pub fn same_cause(a: &FailureCause, b: &FailureCause) -> bool {
    a.kind == b.kind
        && a.error.code == b.error.code
        && a.error.reason == b.error.reason
        && a.error.message == b.error.message
}

/// Object keys use JavaScript's UTF-16 lexical ordering, not UTF-8 byte ordering.
pub fn sorted_copy(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sorted_copy).collect()),
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), sorted_copy(&map[k])))
                    .collect(),
            )
        }
        Value::Number(n) if n.as_f64() == Some(0.0) => json!(0),
        other => other.clone(),
    }
}
pub fn canonical_json(value: &Value) -> Result<String> {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            Ok(format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| Ok(format!(
                        "{}:{}",
                        crate::json::JsonString::from(key.as_str()).to_json(),
                        canonical_json(&map[key])?
                    )))
                    .collect::<Result<Vec<_>>>()?
                    .join(",")
            ))
        }
        Value::Array(values) => Ok(format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Result<Vec<_>>>()?
                .join(",")
        )),
        primitive => crate::json::stringify(primitive),
    }
}
pub fn principal_key(principal: &Value) -> Result<String> {
    canonical_json(principal)
}
pub fn principal_json(principal: &Value) -> Result<String> {
    crate::json::stringify(principal)
}
pub fn same_principal(principal: Option<&Value>, other: &Value) -> Result<bool> {
    Ok(match principal {
        Some(principal) => principal_key(principal)? == principal_key(other)?,
        None => false,
    })
}
pub fn join_key(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| p.replace('\\', "\\\\").replace('|', "\\|"))
        .collect::<Vec<_>>()
        .join("|")
}

pub struct Validator {
    inner: pi_rs_schema::Validator,
    schema: Value,
}
impl Validator {
    pub fn compile(schema: &Value) -> Result<Self> {
        let inner = pi_rs_schema::compile(schema).map_err(|e| Error::Invalid(e.to_string()))?;
        Ok(Self {
            inner,
            schema: schema.clone(),
        })
    }
    pub fn check(&self, value: &Value, reason: &str) -> Result<()> {
        let value = schema_value_with_bytes(value, &self.schema);
        if self.inner.check(&value) {
            return Ok(());
        }
        let errors = self.inner.errors(&value);
        let first = errors
            .first()
            .ok_or_else(|| Error::Invalid("validator rejected without explanation".into()))?;
        let path = if first.path.is_empty() {
            "/"
        } else {
            &first.path
        };
        Err(ChordError::invalid(
            reason,
            format!(
                "{reason}{}: {}",
                if path == "/" { "" } else { path },
                first.message
            ),
            Some(json!({"path":path})),
        )
        .into())
    }
}

fn schema_value_with_bytes(value: &Value, schema: &Value) -> pi_rs_schema::Value {
    let Some(obj) = schema.as_object() else {
        return pi_rs_schema::Value::from(value);
    };
    if matches!(obj.get("type").and_then(Value::as_str), Some("bytes")) {
        if let Value::Array(items) = value {
            let mut bytes = Vec::with_capacity(items.len());
            let mut all_u8 = true;
            for item in items {
                if let Some(n) = item.as_u64().and_then(|v| u8::try_from(v).ok()) {
                    bytes.push(n);
                } else {
                    all_u8 = false;
                    break;
                }
            }
            if all_u8 {
                return pi_rs_schema::Value::Bytes(bytes);
            }
        }
        return pi_rs_schema::Value::from(value);
    }
    if let Some(branches) = obj.get("anyOf").and_then(Value::as_array) {
        for branch in branches {
            let converted = schema_value_with_bytes(value, branch);
            if matches!(converted, pi_rs_schema::Value::Bytes(_)) {
                return converted;
            }
        }
    }
    match value {
        Value::Object(map) => {
            let props = obj.get("properties").and_then(Value::as_object);
            let additional = obj.get("additionalProperties");
            let mut out = Vec::with_capacity(map.len());
            for (k, v) in map {
                let converted = if let Some(prop_schema) = props.and_then(|p| p.get(k)) {
                    schema_value_with_bytes(v, prop_schema)
                } else if let Some(add_schema) = additional {
                    schema_value_with_bytes(v, add_schema)
                } else {
                    pi_rs_schema::Value::from(v)
                };
                out.push((k.clone(), converted));
            }
            pi_rs_schema::Value::Object(out.into_iter().collect())
        }
        Value::Array(items) => {
            if let Some(item_schema) = obj.get("items") {
                pi_rs_schema::Value::Array(
                    items
                        .iter()
                        .map(|v| schema_value_with_bytes(v, item_schema))
                        .collect(),
                )
            } else {
                pi_rs_schema::Value::from(value)
            }
        }
        _ => pi_rs_schema::Value::from(value),
    }
}

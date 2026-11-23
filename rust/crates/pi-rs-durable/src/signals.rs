//! One-shot, versioned signals stored in live task memos.

use std::sync::{Arc, OnceLock};

use pi_rs_schema::{Validator, compile};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::errors::BoxError;
use crate::ids::TaskId;
use crate::storage_types::{
    JsonObject, SubmissionRecord, SubmissionStatus, TaskRecord, TaskState, WakeReason,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SignalKey {
    pub signal: String,
    pub key: String,
}

pub type SignalMigration = Arc<dyn Fn(&Value, u64) -> Result<Value, BoxError> + Send + Sync>;

pub struct SignalDefinition {
    pub name: String,
    pub version: u64,
    pub schema: Value,
    pub migrate: Option<SignalMigration>,
}

pub struct SignalToken {
    name: String,
    version: u64,
    validator: Validator,
    migrate: Option<SignalMigration>,
}

impl SignalToken {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub const fn version(&self) -> u64 {
        self.version
    }

    pub fn key(&self, key: impl Into<String>) -> Result<SignalKey, SignalError> {
        let key = key.into();
        if key.is_empty() {
            return Err(SignalError::EmptyKey(self.name.clone()));
        }
        Ok(SignalKey {
            signal: self.name.clone(),
            key,
        })
    }

    fn accepts(&self, value: &Value) -> bool {
        self.validator.check(&pi_rs_schema::Value::from(value))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SignalError {
    #[error("Signal name {0:?} must be non-empty, without \"/\"")]
    Name(String),
    #[error("Signal {0} version must be a positive integer")]
    Version(String),
    #[error("Signal {name} schema admits non-JSON values ({found})")]
    NonJson { name: String, found: String },
    #[error(transparent)]
    Compile(#[from] pi_rs_schema::CompileError),
    #[error("Signal {0} key must be non-empty")]
    EmptyKey(String),
    #[error("Signal {name} value has version {stored}, newer than {defined}")]
    Newer {
        name: String,
        stored: u64,
        defined: u64,
    },
    #[error("Signal {name} has no migration from version {stored}")]
    NoMigration { name: String, stored: u64 },
    #[error("Signal {name} migration from version {stored} returned an invalid value")]
    InvalidMigration { name: String, stored: u64 },
    #[error("Signal {0} has an invalid stored memo")]
    Memo(String),
    #[error(transparent)]
    Callback(#[from] BoxError),
    #[error("Task {0} cannot sleep without until or signals")]
    EmptySleep(TaskId),
    #[error("Task {task} cannot sleep on signal {signal} key {key}: it {reason}")]
    UsedKey {
        task: TaskId,
        signal: String,
        key: String,
        reason: &'static str,
    },
    #[error("Sleep operation requires a sleeping task")]
    NotSleeping,
}

pub fn define_signal(definition: SignalDefinition) -> Result<SignalToken, SignalError> {
    let SignalDefinition {
        name,
        version,
        schema,
        migrate,
    } = definition;
    if name.is_empty() || name.contains('/') {
        return Err(SignalError::Name(name));
    }
    if version == 0 {
        return Err(SignalError::Version(name));
    }
    if let Some(found) = non_json_type(&schema) {
        return Err(SignalError::NonJson {
            name,
            found: found.to_owned(),
        });
    }
    Ok(SignalToken {
        name,
        version,
        validator: compile(&schema)?,
        migrate,
    })
}

fn non_json_type(schema: &Value) -> Option<&str> {
    const CHILDREN: &[&str] = &[
        "items",
        "prefixItems",
        "additionalItems",
        "unevaluatedItems",
        "contains",
        "anyOf",
        "oneOf",
        "allOf",
        "not",
        "if",
        "then",
        "else",
        "additionalProperties",
        "unevaluatedProperties",
        "propertyNames",
    ];
    const MAPS: &[&str] = &[
        "properties",
        "patternProperties",
        "dependentSchemas",
        "dependencies",
        "$defs",
        "definitions",
    ];
    fn is_non_json(kind: &str) -> bool {
        matches!(kind, "bytes" | "bigint" | "undefined")
    }
    if let Some(kind) = schema.get("type") {
        let found = kind.as_str().filter(|kind| is_non_json(kind)).or_else(|| {
            kind.as_array()?
                .iter()
                .filter_map(Value::as_str)
                .find(|kind| is_non_json(kind))
        });
        if found.is_some() {
            return found;
        }
    }
    for key in CHILDREN {
        if let Some(child) = schema.get(*key) {
            let found = if let Some(children) = child.as_array() {
                children.iter().find_map(non_json_type)
            } else {
                non_json_type(child)
            };
            if found.is_some() {
                return found;
            }
        }
    }
    MAPS.iter()
        .filter_map(|key| schema.get(*key)?.as_object())
        .flat_map(JsonObject::values)
        .find_map(non_json_type)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SignalState {
    Value { value: Value },
    Expired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalResult {
    Delivered,
    Duplicate,
    Invalid,
    Closed,
    NotFound,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SignalDelivery {
    pub result: SignalResult,
    pub record: Option<TaskRecord>,
}

pub fn is_settled(record: &SubmissionRecord) -> bool {
    matches!(
        record.status,
        SubmissionStatus::Done | SubmissionStatus::Unanswered
    )
}

pub fn submission_settled() -> Result<&'static SignalToken, &'static SignalError> {
    static TOKEN: OnceLock<Result<SignalToken, SignalError>> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let int = || json!({"type":"integer"});
        let str = || json!({"type":"string"});
        let variant = |kind, status, entry, answer, reason| {
            let mut required = vec!["id", "conversationId", "type", "status"];
            if entry { required.push("entry"); }
            if answer { required.push("answer"); }
            if reason { required.push("reason"); }
            let mut properties = json!({
                "id": int(), "conversationId": int(), "type": {"const": kind}, "status": {"const": status},
                "requestId": str(), "notify": {"type":"object", "required":["taskId","key"], "additionalProperties":false,
                    "properties": {"taskId": int(), "key": str()}}
            });
            if entry || (kind == "input" && reason) { properties["entry"] = int(); }
            if answer { properties["answer"] = int(); }
            if reason { properties["reason"] = str(); }
            json!({"type":"object", "properties":properties, "required":required})
        };
        define_signal(SignalDefinition {
            name: "pi.submission-settled".into(), version: 1, migrate: None,
            schema: json!({"anyOf": [
                variant("input", "done", true, true, false),
                variant("write", "done", true, false, false),
                variant("continue", "done", false, true, false),
                variant("input", "unanswered", false, false, true),
                variant("write", "unanswered", false, false, true),
                variant("continue", "unanswered", false, false, true),
            ]}),
        })
    }).as_ref()
}

fn memo_name(signal: &str, key: &str) -> String {
    format!("pi.signal.{signal}/{key}")
}
fn flag(stored: &Value, key: &str) -> bool {
    stored.get(key) == Some(&Value::Bool(true))
}

/// Delivery does not mutate a borrowed record; only a durable transaction may adopt the replacement.
pub fn deliver_signal(
    record: Option<&TaskRecord>,
    token: &SignalToken,
    key: &str,
    value: Value,
    now: u64,
) -> SignalDelivery {
    let result = |result| SignalDelivery {
        result,
        record: None,
    };
    let Some(record) = record else {
        return result(SignalResult::NotFound);
    };
    if record.abort_requested
        || matches!(
            record.state,
            TaskState::Terminal { .. } | TaskState::Completing { .. }
        )
    {
        return result(SignalResult::Closed);
    }
    if key.is_empty() || !token.accepts(&value) {
        return result(SignalResult::Invalid);
    }
    let name = memo_name(&token.name, key);
    if let Some(stored) = record.memos.as_ref().and_then(|memos| memos.get(&name)) {
        return result(if flag(stored, "expired") {
            SignalResult::Closed
        } else {
            SignalResult::Duplicate
        });
    }
    let expired = matches!(&record.state,
        TaskState::Sleeping { until: Some(until), signals: Some(keys), .. }
            if *until <= now && keys.iter().any(|listed| listed.signal == token.name && listed.key == key));
    let (result, memo) = if expired {
        (SignalResult::Closed, json!({"expired":true}))
    } else {
        (
            SignalResult::Delivered,
            json!({"version":token.version, "value":value}),
        )
    };
    let mut record = record.clone();
    record
        .memos
        .get_or_insert_with(JsonObject::new)
        .insert(name, memo);
    SignalDelivery {
        result,
        record: Some(record),
    }
}

pub fn read_signal(
    memos: Option<&JsonObject>,
    token: &SignalToken,
    key: &str,
) -> Result<Option<SignalState>, SignalError> {
    let Some(stored) = memos.and_then(|memos| memos.get(&memo_name(&token.name, key))) else {
        return Ok(None);
    };
    if flag(stored, "expired") {
        return Ok(Some(SignalState::Expired));
    }
    let (Some(version), Some(value)) = (
        stored.get("version").and_then(Value::as_u64),
        stored.get("value"),
    ) else {
        return Err(SignalError::Memo(token.name.clone()));
    };
    match version.cmp(&token.version) {
        std::cmp::Ordering::Equal => Ok(Some(SignalState::Value {
            value: value.clone(),
        })),
        std::cmp::Ordering::Greater => Err(SignalError::Newer {
            name: token.name.clone(),
            stored: version,
            defined: token.version,
        }),
        std::cmp::Ordering::Less => {
            let migrate = token
                .migrate
                .as_ref()
                .ok_or_else(|| SignalError::NoMigration {
                    name: token.name.clone(),
                    stored: version,
                })?;
            let value = migrate(value, version)?;
            if !token.accepts(&value) {
                return Err(SignalError::InvalidMigration {
                    name: token.name.clone(),
                    stored: version,
                });
            }
            Ok(Some(SignalState::Value { value }))
        }
    }
}

pub fn check_sleep(
    state: &TaskState,
    memos: Option<&JsonObject>,
    task: TaskId,
) -> Result<(), SignalError> {
    let TaskState::Sleeping { until, signals, .. } = state else {
        return Err(SignalError::NotSleeping);
    };
    let signals = signals.as_deref().unwrap_or_default();
    if until.is_none() && signals.is_empty() {
        return Err(SignalError::EmptySleep(task));
    }
    if let Some(memos) = memos {
        for SignalKey { signal, key } in signals {
            let Some(stored) = memos.get(&memo_name(signal, key)) else {
                continue;
            };
            let reason = if flag(stored, "expired") {
                "expired"
            } else if flag(stored, "consumed") {
                "already woke a wait"
            } else {
                continue;
            };
            return Err(SignalError::UsedKey {
                task,
                signal: signal.clone(),
                key: key.clone(),
                reason,
            });
        }
    }
    Ok(())
}

fn ready_key<'a, 'b>(
    signals: &'a [SignalKey],
    memos: Option<&'b JsonObject>,
) -> Option<(&'a SignalKey, String, &'b Value)> {
    let memos = memos?;
    signals.iter().find_map(|listed| {
        let name = memo_name(&listed.signal, &listed.key);
        let stored = memos.get(&name)?;
        (!flag(stored, "expired") && !flag(stored, "consumed")).then_some((listed, name, stored))
    })
}

pub fn sleep_due(state: &TaskState, memos: Option<&JsonObject>, now: u64) -> bool {
    let TaskState::Sleeping { until, signals, .. } = state else {
        return false;
    };
    until.is_some_and(|until| until <= now)
        || ready_key(signals.as_deref().unwrap_or_default(), memos).is_some()
}

#[derive(Clone, Debug, PartialEq)]
pub struct SleepWake {
    pub wake: WakeReason,
    /// Patch only: unaffected memos remain in the task record.
    pub memos: JsonObject,
}

pub fn sleep_wake(state: &TaskState, memos: Option<&JsonObject>, now: u64) -> Option<SleepWake> {
    let TaskState::Sleeping { until, signals, .. } = state else {
        return None;
    };
    let signals = signals.as_deref().unwrap_or_default();
    if let Some((listed, name, stored)) = ready_key(signals, memos) {
        let mut consumed = stored.as_object()?.clone();
        consumed.insert("consumed".into(), Value::Bool(true));
        return Some(SleepWake {
            wake: WakeReason::Signal {
                signal: listed.signal.clone(),
                key: listed.key.clone(),
            },
            memos: [(name, Value::Object(consumed))].into_iter().collect(),
        });
    }
    if (*until)? > now {
        return None;
    }
    let patch = signals.iter().filter_map(|listed| {
        let name = memo_name(&listed.signal, &listed.key);
        (!memos.is_some_and(|memos| memos.contains_key(&name)))
            .then(|| (name, json!({"expired":true})))
    });
    Some(SleepWake {
        wake: if signals.is_empty() {
            WakeReason::Time
        } else {
            WakeReason::Expired
        },
        memos: patch.collect(),
    })
}

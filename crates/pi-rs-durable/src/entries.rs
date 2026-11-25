//! Typed tokens for the immutable entry-kind discriminator.

use std::borrow::Cow;
use std::marker::PhantomData;

use crate::storage_types::EntryRecord;
use crate::tools::ToolDiagnostic;
use serde::de::DeserializeOwned;
use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry<T = ()> {
    kind: Cow<'static, str>,
    payload: PhantomData<fn() -> T>,
}

#[derive(Debug, thiserror::Error)]
#[error("Entry kind must be a non-empty string")]
pub struct EmptyEntryKind;

pub fn define_entry<T>(kind: impl Into<Cow<'static, str>>) -> Result<Entry<T>, EmptyEntryKind> {
    let kind = kind.into();
    if kind.is_empty() {
        return Err(EmptyEntryKind);
    }
    Ok(Entry {
        kind,
        payload: PhantomData,
    })
}

impl<T> Entry<T> {
    const fn builtin(kind: &'static str) -> Self {
        Self {
            kind: Cow::Borrowed(kind),
            payload: PhantomData,
        }
    }

    pub fn kind(&self) -> &str {
        self.kind.as_ref()
    }

    pub fn matches(&self, entry: Option<&EntryRecord>) -> bool {
        entry.is_some_and(|entry| entry.kind == self.kind)
    }

    /// Decode the application payload only for records of this token's kind.
    pub fn payload(&self, entry: &EntryRecord) -> Result<Option<T>, serde_json::Error>
    where
        T: DeserializeOwned,
    {
        if !self.matches(Some(entry)) {
            return Ok(None);
        }
        T::deserialize(entry.data.as_ref().unwrap_or(&Value::Null)).map(Some)
    }
}

pub const USER_ENTRY: Entry = Entry::builtin("pi.user");
pub const ASSISTANT_ENTRY: Entry = Entry::builtin("pi.assistant");
pub const SYSTEM_ENTRY: Entry = Entry::builtin("pi.system");
pub const TOOL_RESULT_ENTRY: Entry<ToolResultData> = Entry::builtin("pi.tool-result");
pub const RESET_ENTRY: Entry = Entry::builtin("pi.reset");
pub const COMPACTION_ENTRY: Entry<CompactionData> = Entry::builtin("pi.compaction");

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolResultData {
    pub diagnostics: Vec<ToolDiagnostic>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CompactionData {
    pub reason: CompactionReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

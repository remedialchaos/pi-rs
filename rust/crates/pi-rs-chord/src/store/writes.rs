use super::{ColumnRecord, SetNode, TableState, WhereNode};
use crate::json::JsonValue;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum AlterEdit {
    Rename { from: String, to: String },
    Add { name: String, column: ColumnRecord },
    Drop { name: String },
    Json { name: String, column: ColumnRecord },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentRecord {
    pub id: i64,
    pub owner: String,
    pub namespace: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub history: DocumentHistory,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub revision: i64,
    pub version: i64,
    pub fingerprint: String,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DocumentHistory {
    None,
    Latest,
    Rewindable,
}
impl DocumentHistory {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Latest => "latest",
            Self::Rewindable => "rewindable",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentBase {
    pub version: i64,
    pub fingerprint: String,
    #[serde(deserialize_with = "json_value")]
    pub value: JsonValue,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum DocumentContent {
    Base {
        version: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        fingerprint: Option<String>,
        #[serde(deserialize_with = "json_value")]
        value: JsonValue,
    },
    Delta {
        version: i64,
        #[serde(deserialize_with = "json_values")]
        ops: Vec<JsonValue>,
    },
}
// Internally tagged enums buffer serde data. RawValue's JSON-specific newtype
// cannot deserialize that buffer, so stored wire cells enter through Value.
fn json_value<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<JsonValue, D::Error> {
    Value::deserialize(d).map(JsonValue::from_serde)
}
fn json_values<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<JsonValue>, D::Error> {
    Vec::<Value>::deserialize(d)
        .map(|values| values.into_iter().map(JsonValue::from_serde).collect())
}
impl DocumentContent {
    pub(crate) fn version(&self) -> i64 {
        match self {
            Self::Base { version, .. } | Self::Delta { version, .. } => *version,
        }
    }
    pub(crate) fn content(&self) -> crate::Result<(&'static str, String)> {
        Ok(match self {
            Self::Base { value, .. } => ("base", serde_json::to_string(value)?),
            Self::Delta { ops, .. } => ("delta", serde_json::to_string(ops)?),
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum StoreWrite {
    Table {
        table: String,
        version: i64,
        state: TableState,
        columns: IndexMap<String, ColumnRecord>,
        key: Vec<String>,
        fingerprint: String,
    },
    Index {
        table: String,
        name: String,
        columns: Vec<String>,
    },
    DropIndex {
        table: String,
        name: String,
    },
    Alter {
        table: String,
        version: i64,
        edit: AlterEdit,
    },
    Put {
        table: String,
        row: IndexMap<String, Value>,
        version: i64,
        columns: IndexMap<String, i64>,
    },
    Update {
        table: String,
        key: IndexMap<String, Value>,
        set: SetNode,
        columns: IndexMap<String, i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<i64>,
    },
    Delete {
        table: String,
        key: IndexMap<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<i64>,
    },
    UpdateWhere {
        table: String,
        r#where: WhereNode,
        set: SetNode,
        columns: IndexMap<String, i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<i64>,
    },
    DeleteWhere {
        table: String,
        r#where: WhereNode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<i64>,
    },
    #[serde(rename = "document.create")]
    DocumentCreate {
        record: DocumentRecord,
        #[serde(skip_serializing_if = "Option::is_none")]
        base: Option<DocumentBase>,
    },
    #[serde(rename = "document.change")]
    DocumentChange {
        id: i64,
        revision: i64,
        content: DocumentContent,
    },
    #[serde(rename = "document.retire")]
    DocumentRetire {
        id: i64,
        revision: i64,
    },
    #[serde(rename = "document.revision")]
    DocumentRevision {
        id: i64,
        seq: i64,
        content: DocumentContent,
    },
}
impl StoreWrite {
    pub fn update_where(
        table: impl Into<String>,
        r#where: WhereNode,
        set: SetNode,
        columns: IndexMap<String, i64>,
    ) -> Self {
        Self::UpdateWhere {
            table: table.into(),
            r#where,
            set,
            columns,
            count: None,
            version: None,
        }
    }
}

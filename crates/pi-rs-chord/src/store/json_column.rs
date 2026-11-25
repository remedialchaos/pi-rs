//! JSON cells are migrated on access, never rewritten or walked on write.
use super::{
    ColumnRecord,
    table::{invalid, safe},
};
use crate::{Result, context::lock, contracts, data::Validator, json::JsonValue};
use std::{
    fmt,
    sync::{Arc, Mutex},
};
pub type JsonMigration = Arc<dyn Fn(JsonValue, i64) -> Result<JsonValue> + Send + Sync>;
#[derive(Clone)]
pub struct JsonColumn(Arc<Inner>);
struct Inner {
    schema: JsonValue,
    version: i64,
    fingerprint: String,
    migrate: Option<JsonMigration>,
    validator: Mutex<Option<Validator>>,
}
impl fmt::Debug for JsonColumn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonColumn")
            .field("version", &self.0.version)
            .field("fingerprint", &self.0.fingerprint)
            .finish_non_exhaustive()
    }
}
impl JsonColumn {
    pub fn new(schema: JsonValue, version: i64, migrate: Option<JsonMigration>) -> Result<Self> {
        safe(version, 1)?;
        let snapshot = contracts::snapshot_schema(&schema, "json(...)")?;
        if snapshot.bytes {
            return Err(invalid("json column schema must contain JSON only"));
        }
        Ok(Self(Arc::new(Inner {
            schema: snapshot.schema,
            version,
            fingerprint: contracts::fingerprint_of(&snapshot.normal)?,
            migrate,
            validator: Mutex::new(None),
        })))
    }
    pub fn version(&self) -> i64 {
        self.0.version
    }
    pub fn fingerprint(&self) -> &str {
        &self.0.fingerprint
    }
    pub fn record(&self) -> ColumnRecord {
        ColumnRecord::json(self.version(), self.fingerprint())
    }
    pub fn read(&self, text: &str, version: i64) -> Result<JsonValue> {
        let value = JsonValue::from_serde(serde_json::from_str(text)?);
        if version == self.version() {
            return Ok(value);
        }
        let unavailable = |message: String| crate::data::ChordError::new("unavailable", message);
        if version > self.version() || version < 1 {
            return Err(unavailable(format!(
                "json cell: unreadable version {version}, current {}",
                self.version()
            ))
            .into());
        }
        let migrate = self.0.migrate.as_ref().ok_or_else(|| {
            unavailable(format!(
                "json cell: no migrate from {version} to {}",
                self.version()
            ))
        })?;
        let migrated = migrate(value, version)
            .map_err(|e| unavailable(format!("json cell: migration from {version} failed: {e}")))?;
        let mut validator = lock(&self.0.validator);
        if validator.is_none() {
            *validator = Some(Validator::compile(&self.0.schema.to_serde()?)?);
        }
        if let Some(validator) = validator.as_ref() {
            validator
                .check(&migrated.to_serde()?, "json migration")
                .map_err(|_| {
                    unavailable(format!(
                        "json cell: migrated version {version} does not match schema"
                    ))
                })?;
        }
        Ok(migrated)
    }
}

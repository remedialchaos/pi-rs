use crate::{
    Error, Result,
    context::lock,
    contracts::{self, HostKind},
    data::Validator,
    delta::Op,
    json::JsonValue,
};
use std::sync::{Arc, Mutex};
#[derive(Clone)]
pub enum KeyedBy {
    Singleton,
    String,
    Host(HostKind),
}
type Initial = Arc<dyn Fn() -> Result<JsonValue> + Send + Sync>;
type Migrate = Arc<dyn Fn(JsonValue, u64) -> Result<JsonValue> + Send + Sync>;
type Checkpoint = Arc<dyn Fn(&JsonValue, &[Op], usize) -> bool + Send + Sync>;
pub struct Definition {
    pub(super) name: String,
    pub(super) schema: JsonValue,
    pub(super) version: u64,
    pub(super) fingerprint: String,
    pub(super) keyed: KeyedBy,
    pub(super) persist: bool,
    initial: Initial,
    migrate: Option<Migrate>,
    checkpoint: Checkpoint,
    checked: Mutex<bool>,
    validator: Mutex<Option<Validator>>,
}
impl Definition {
    pub fn new(
        name: impl Into<String>,
        schema: JsonValue,
        version: u64,
        initial: impl Fn() -> Result<JsonValue> + Send + Sync + 'static,
    ) -> Result<Self> {
        let name = name.into();
        contracts::check_name(&name, "A state name")?;
        let snapshot = contracts::snapshot_schema(&schema, &name)?;
        if snapshot.bytes {
            return Err(Error::Invalid(format!(
                "{name}: document schemas must be JSON-only"
            )));
        }
        Ok(Self {
            name,
            schema: snapshot.schema,
            version,
            fingerprint: contracts::fingerprint_of(&snapshot.normal)?,
            keyed: KeyedBy::Singleton,
            persist: false,
            initial: Arc::new(initial),
            migrate: None,
            checkpoint: Arc::new(|_, _, _| false),
            checked: Mutex::new(false),
            validator: Mutex::new(None),
        })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn version(&self) -> u64 {
        self.version
    }
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    pub fn is_persisted(&self) -> bool {
        self.persist
    }
    pub fn is_singleton(&self) -> bool {
        matches!(self.keyed, KeyedBy::Singleton)
    }
    pub(crate) fn validate(&self, value: &JsonValue) -> Result<()> {
        self.check(value, "state")
    }
    pub fn keyed(mut self, keyed: KeyedBy) -> Self {
        self.keyed = keyed;
        self
    }
    pub fn persisted(
        mut self,
        persist: bool,
        rule: impl Fn(&JsonValue, &[Op], usize) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.persist = persist;
        self.checkpoint = Arc::new(rule);
        self
    }
    pub fn migrating(
        mut self,
        migrate: impl Fn(JsonValue, u64) -> Result<JsonValue> + Send + Sync + 'static,
    ) -> Self {
        self.migrate = Some(Arc::new(migrate));
        self
    }
    fn check(&self, value: &JsonValue, what: &str) -> Result<()> {
        let mut validator = lock(&self.validator);
        if validator.is_none() {
            *validator = Some(Validator::compile(&self.schema.to_serde()?)?)
        }
        if let Some(validator) = validator.as_ref() {
            validator.check(&value.to_serde()?, what).map_err(|_| {
                Error::Invalid(format!("{}: {what} does not match the schema", self.name))
            })?;
        }
        Ok(())
    }
    pub fn initial(&self) -> Result<JsonValue> {
        let value = (self.initial)()?;
        let mut checked = lock(&self.checked);
        if !*checked {
            self.check(&value, "initial()")?;
            *checked = true;
        }
        Ok(value)
    }
    pub fn migrate(&self, old: JsonValue, from: u64) -> Result<JsonValue> {
        let migrate = self.migrate.as_ref().ok_or_else(|| {
            Error::Invalid(format!(
                "{}: no migrate from version {from} to {}",
                self.name, self.version
            ))
        })?;
        let value = migrate(old, from)?;
        self.check(&value, &format!("migrate() from version {from}"))?;
        Ok(value)
    }
    pub(super) fn checkpoint(&self, value: &JsonValue, ops: &[Op], deltas: usize) -> bool {
        (self.checkpoint)(value, ops, deltas)
    }
    pub(crate) fn compatible(&self, next: &Definition) -> Result<()> {
        let same_keyed = matches!(
            (&self.keyed, &next.keyed),
            (KeyedBy::Singleton, KeyedBy::Singleton) | (KeyedBy::String, KeyedBy::String)
        ) || matches!((&self.keyed,&next.keyed),(KeyedBy::Host(a),KeyedBy::Host(b))if a.kind==b.kind);
        if !same_keyed
            || self.persist != next.persist
            || next.version < self.version
            || next.version == self.version && next.fingerprint != self.fingerprint
        {
            return Err(Error::Invalid(format!(
                "{}: incompatible document declaration or schema changed without a version bump",
                self.name
            )));
        }
        Ok(())
    }
    pub(super) fn readable(&self, version: u64, fingerprint: &str) -> Result<()> {
        if version > self.version {
            return Err(Error::Invalid(format!(
                "{}: stored at version {version}, newer than the code's {}",
                self.name, self.version
            )));
        }
        if version == self.version && fingerprint != self.fingerprint {
            return Err(Error::Invalid(format!(
                "{}: stored with another schema at version {version}",
                self.name
            )));
        }
        Ok(())
    }
}

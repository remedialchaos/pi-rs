use super::{Definition, KeyedBy, Staged};
use crate::{
    Error, Result,
    delta::{Op, Revision, apply},
    json::JsonValue,
    sqlite::{SqliteDatabase, SqliteFuture},
    store::{
        DocumentBase, DocumentContent, DocumentHistory, DocumentRecord, Step, StoreEngine,
        StoreWrite, WriteOrder,
    },
};
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::Arc;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    pub id: i64,
    pub revision: i64,
    pub version: u64,
    pub deltas_since_base: usize,
}
#[derive(Clone)]
pub struct DocumentStore {
    pub(super) database: SqliteDatabase,
    owner: Arc<str>,
    engine: Option<StoreEngine>,
}
pub(super) struct Loaded {
    pub stored: Stored,
    pub value: JsonValue,
    pub fingerprint: String,
    pub host: Option<String>,
}
pub(super) struct Plan {
    pub key: String,
    pub definition: Arc<Definition>,
    pub stored: Option<Stored>,
    pub retire: bool,
    pub value: Option<Revision>,
    pub ops: Vec<Op>,
    pub base: bool,
    pub text: Option<String>,
}
impl Plan {
    pub fn serialize(&mut self) -> Result<()> {
        if let Some(value) = &self.value {
            self.base = self.retire
                || self.stored.as_ref().is_none_or(|s| {
                    s.version != self.definition.version
                        || self.definition.checkpoint(
                            &value.to_json(),
                            &self.ops,
                            s.deltas_since_base,
                        )
                });
            self.text = Some(serde_json::to_string(&if self.base {
                value.to_json()
            } else {
                JsonValue::Array(self.ops.iter().map(Op::to_json).collect())
            })?);
        }
        Ok(())
    }
}
impl DocumentStore {
    pub fn new(database: SqliteDatabase, owner: impl Into<Arc<str>>) -> Self {
        Self {
            database,
            owner: owner.into(),
            engine: None,
        }
    }
    pub fn from_engine(engine: StoreEngine, owner: impl Into<Arc<str>>) -> Self {
        Self {
            database: engine.database().clone(),
            owner: owner.into(),
            engine: Some(engine),
        }
    }
    pub(super) fn batch(&self) -> Option<crate::state::Batch> {
        self.engine.as_ref().map(StoreEngine::batch)
    }
    pub fn database(&self) -> &SqliteDatabase {
        &self.database
    }
    pub(super) fn notify(&self, seq: i64) {
        if let Some(engine) = &self.engine {
            engine.publish(seq);
        }
    }
    pub(super) fn read(
        &self,
        name: String,
        key: String,
        load: bool,
    ) -> SqliteFuture<Option<Loaded>> {
        let store = self.clone();
        self.database
            .call(move |c| store.read_in(c, &name, &key, load))
    }
    pub(super) fn read_in(
        &self,
        c: &Connection,
        name: &str,
        key: &str,
        load: bool,
    ) -> Result<Option<Loaded>> {
        let row=c.query_row("SELECT id,revision,version,fingerprint,host FROM chord_documents WHERE owner=? AND namespace='chord' AND kind=? AND key=? AND retired_at IS NULL",params![&*self.owner,name,key],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?,r.get::<_,Option<String>>(4)?))).optional()?;
        let Some((id, revision, version, fingerprint, host)) = row else {
            return Ok(None);
        };
        let version = u64::try_from(version)
            .map_err(|_| Error::Invalid("negative document version".into()))?;
        let (mut value, mut deltas) = (JsonValue::Null, 0);
        if load {
            let mut statement=c.prepare_cached("SELECT kind,version,content FROM chord_revisions WHERE document_id=? AND seq >= (SELECT MAX(seq) FROM chord_revisions WHERE document_id=? AND kind='base') ORDER BY seq")?;
            let rows = statement.query_map([id, id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            let mut found_base = false;
            for row in rows {
                let (kind, v, text) = row?;
                if u64::try_from(v).ok() != Some(version) {
                    return Err(Error::Invalid("document revision version mismatch".into()));
                }
                let data: JsonValue = serde_json::from_str(&text)?;
                if kind == "base" {
                    value = data;
                    found_base = true;
                    deltas = 0;
                } else {
                    if !found_base {
                        return Err(Error::Invalid("document has no base".into()));
                    }
                    let JsonValue::Array(wire) = data else {
                        return Err(Error::Invalid("invalid stored delta".into()));
                    };
                    let ops = wire
                        .iter()
                        .map(Op::from_json)
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    value = apply(value, &ops)?;
                    deltas += 1;
                }
            }
            if !found_base {
                return Err(Error::Invalid("document has no base".into()));
            }
        }
        Ok(Some(Loaded {
            stored: Stored {
                id,
                revision,
                version,
                deltas_since_base: deltas,
            },
            value,
            fingerprint,
            host,
        }))
    }
    pub(super) fn keys(&self, name: String, after: Option<String>) -> SqliteFuture<Vec<String>> {
        let owner = self.owner.clone();
        self.database.call(move|c|{let mut s=c.prepare_cached("SELECT key FROM chord_documents WHERE owner=? AND namespace='chord' AND kind=? AND retired_at IS NULL AND (? IS NULL OR key>?) ORDER BY key LIMIT 100")?;Ok(s.query_map(params![&*owner,name,after,after],|r|r.get(0))?.collect::<std::result::Result<Vec<_>,_>>()?)})
    }
    pub(super) fn apply(
        &self,
        c: &Connection,
        plans: &[Plan],
        staged: &[Arc<dyn Staged>],
    ) -> Result<(
        Vec<Option<Stored>>,
        i64,
        Vec<crate::store::live_query::LiveRun>,
    )> {
        let mut step = Step::new(c)?;
        let mut results = vec![None; plans.len()];
        // Global phases free live addresses before replacements, and all mutations share one outbox sequence.
        for order in [WriteOrder::Retire, WriteOrder::Create, WriteOrder::Write] {
            for (i, p) in plans.iter().enumerate() {
                if order == WriteOrder::Retire {
                    if p.retire
                        && let Some(old) = &p.stored
                    {
                        step.apply(StoreWrite::DocumentRetire {
                            id: old.id,
                            revision: old.revision,
                        })?;
                    }
                    continue;
                }
                let Some(value) = &p.value else { continue };
                let create = p.retire || p.stored.is_none();
                if (order == WriteOrder::Create) != create {
                    continue;
                }
                let def = &p.definition;
                let version = i64::try_from(def.version)
                    .map_err(|_| Error::Invalid("document version overflow".into()))?;
                let text = p
                    .text
                    .as_ref()
                    .ok_or_else(|| Error::Invalid("document write not serialized".into()))?;
                if create {
                    let id = step.mint()?;
                    let base = DocumentBase {
                        version,
                        fingerprint: def.fingerprint.clone(),
                        value: value.to_json(),
                    };
                    let record = DocumentRecord {
                        id,
                        owner: self.owner.to_string(),
                        namespace: "chord".into(),
                        kind: def.name.clone(),
                        key: (!p.key.is_empty()).then(|| p.key.clone()),
                        history: DocumentHistory::None,
                        created_at: step.seq,
                        retired_at: None,
                        meta: None,
                        host: matches!(def.keyed, KeyedBy::Host(_)).then(|| p.key.clone()),
                        revision: 1,
                        version,
                        fingerprint: def.fingerprint.clone(),
                    };
                    step.apply(StoreWrite::DocumentCreate {
                        record,
                        base: Some(base),
                    })?;
                    results[i] = Some(Stored {
                        id,
                        revision: 1,
                        version: def.version,
                        deltas_since_base: 0,
                    });
                } else {
                    let old = p
                        .stored
                        .as_ref()
                        .ok_or_else(|| Error::Invalid("missing stored revision".into()))?;
                    let content = if p.base {
                        DocumentContent::Base {
                            version,
                            fingerprint: Some(def.fingerprint.clone()),
                            value: serde_json::from_str(text)?,
                        }
                    } else {
                        DocumentContent::Delta {
                            version,
                            ops: serde_json::from_str(text)?,
                        }
                    };
                    step.apply(StoreWrite::DocumentChange {
                        id: old.id,
                        revision: old.revision,
                        content,
                    })?;
                    results[i] = Some(Stored {
                        id: old.id,
                        revision: old.revision + 1,
                        version: def.version,
                        deltas_since_base: if p.base { 0 } else { old.deltas_since_base + 1 },
                    });
                }
            }
            for staged in staged {
                if staged.order() == order {
                    staged.apply(&mut step)?;
                }
            }
        }
        let seq = step.finish()?.seq;
        let runs = self
            .engine
            .as_ref()
            .map_or_else(Vec::new, |engine| engine.prepare_live(c));
        Ok((results, seq, runs))
    }
}

//! Job storage uses Chord's existing document namespaces and outbox steps.
use super::{FinishStep, StoredRun};
use crate::{
    Error, Result,
    json::JsonValue,
    store::{
        Claimant, DocumentBase, DocumentContent, DocumentHistory, DocumentRecord, Step,
        StoreEngine, StoreWrite,
    },
};
use rusqlite::{OptionalExtension, params};
use std::sync::Arc;
struct Owner {
    engine: StoreEngine,
    owner: String,
    claimant: Claimant,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.engine.claims().release(&self.claimant);
    }
}
#[derive(Clone)]
pub struct JobStore(Arc<Owner>);
impl JobStore {
    pub fn new(engine: StoreEngine, owner: impl Into<String>) -> Result<Self> {
        let owner = owner.into();
        let claimant = Claimant::new(format!("jobs of {owner}"));
        engine.claims().documents(&owner, "chord.jobs", &claimant)?;
        if let Err(error) = engine
            .claims()
            .documents(&owner, "chord.job-states", &claimant)
        {
            engine.claims().release(&claimant);
            return Err(error);
        }
        Ok(Self(Arc::new(Owner {
            engine,
            owner,
            claimant,
        })))
    }
    pub async fn load(&self, job: &str) -> Result<Vec<(String, StoredRun, Option<JsonValue>)>> {
        Ok(self
            .load_versions(job, |_| true)
            .await?
            .into_iter()
            .map(|(id, run, state)| (id, run, state.map(|(_, _, value)| value)))
            .collect())
    }
    pub(super) async fn load_versions(
        &self,
        job: &str,
        load_state: impl Fn(&StoredRun) -> bool + Send + 'static,
    ) -> Result<Vec<(String, StoredRun, Option<(u64, String, JsonValue)>)>> {
        let owner = self.0.owner.clone();
        let job = job.to_string();
        self.0.engine.database().call(move |c| {
            let mut statement = c.prepare_cached(
                "SELECT d.key, r.content FROM chord_documents d JOIN chord_revisions r
                 ON r.document_id = d.id AND r.seq = (SELECT MAX(seq) FROM chord_revisions WHERE document_id = d.id)
                 WHERE d.owner = ? AND d.namespace = 'chord.jobs' AND d.kind = ? AND d.retired_at IS NULL ORDER BY d.key"
            )?;
            let rows = statement.query_map(params![owner, job], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut result = Vec::new();
            for row in rows {
                let (id, text) = row?;
                let record: StoredRun = serde_json::from_str(&text)?;
                let state = if load_state(&record) {
                    let header: Option<(i64,i64,String)> = c.query_row(
                        "SELECT id, version, fingerprint FROM chord_documents WHERE owner = ?
                         AND namespace = 'chord.job-states' AND kind = ? AND key = ? AND retired_at IS NULL",
                        params![owner,job,id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    ).optional()?;
                    header.map(|(document, version, fingerprint)| -> Result<_> {
                        let version = u64::try_from(version).map_err(|_| Error::Invalid("negative stored job state version".into()))?;
                        let (value, _) = crate::documents::read_document_revision(c, document, version)?;
                        Ok((version, fingerprint, value))
                    }).transpose()?
                } else {
                    None
                };
                result.push((id, record, state));
            }
            Ok(result)
        }).await
    }
    pub async fn save(
        &self,
        job: &str,
        id: &str,
        record: &StoredRun,
        state: Option<&JsonValue>,
    ) -> Result<()> {
        self.save_defined(job, id, record, state, None).await
    }
    pub(crate) async fn save_defined(
        &self,
        job: &str,
        id: &str,
        record: &StoredRun,
        state: Option<&JsonValue>,
        definition: Option<&crate::documents::Definition>,
    ) -> Result<()> {
        let record = JsonValue::from_serde(serde_json::to_value(record)?);
        let state = state.cloned();
        let owner = self.0.owner.clone();
        let job = job.to_string();
        let id = id.to_string();
        let state_version = i64::try_from(definition.map_or(1, |d| d.version()))
            .map_err(|_| Error::Invalid("job state version exceeds SQLite integer range".into()))?;
        let state_fingerprint = definition
            .map_or("chord.job-state.1", |d| d.fingerprint())
            .to_string();
        let commit=self.0.engine.database().transaction(move |c| {
            let mut step=Step::new(c)?;
            for (namespace,value,version,fingerprint) in [("chord.jobs",Some(record),1,"chord.job-run.1"),("chord.job-states",state,state_version,state_fingerprint.as_str())] {
                let Some(value)=value else {continue;};
                let old=c.query_row("SELECT id,revision FROM chord_documents WHERE owner=? AND namespace=? AND kind=? AND key=? AND retired_at IS NULL",params![owner,namespace,job,id],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?))).optional()?;
                if let Some((document,revision))=old {step.apply(StoreWrite::DocumentChange {id:document,revision,content:DocumentContent::Base {version,fingerprint:Some(fingerprint.into()),value}})?;}
                else {let document=step.mint()?;step.apply(StoreWrite::DocumentCreate {record:DocumentRecord {id:document,owner:owner.clone(),namespace:namespace.into(),kind:job.clone(),key:Some(id.clone()),history:DocumentHistory::None,created_at:step.seq,retired_at:None,meta:None,host:None,revision:1,version,fingerprint:fingerprint.into()},base:Some(DocumentBase {version,fingerprint:fingerprint.into(),value})})?;}
            }step.finish()
        }).await?;
        self.0.engine.publish(commit.seq);
        Ok(())
    }
    pub async fn remove(&self, job: &str, id: &str) -> Result<()> {
        self.retire(job, id, None).await
    }
    pub(super) async fn retire(
        &self,
        job: &str,
        id: &str,
        finish: Option<(FinishStep, super::Outcome)>,
    ) -> Result<()> {
        let owner = self.0.owner.clone();
        let job = job.to_string();
        let id = id.to_string();
        let commit=self.0.engine.database().transaction(move |c| {
            let mut statement=c.prepare_cached("SELECT id,revision FROM chord_documents WHERE owner=? AND namespace IN ('chord.jobs','chord.job-states') AND kind=? AND key=? AND retired_at IS NULL")?;
            let rows=statement.query_map(params![owner,job,id],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let mut step = Step::new(c)?;
            if let Some((finish, outcome)) = finish {
                finish(&outcome, &mut step)?;
            }
            for (id, revision) in rows {
                step.apply(StoreWrite::DocumentRetire { id, revision })?;
            }
            step.finish()
        }).await?;
        self.0.engine.publish(commit.seq);
        Ok(())
    }
    pub fn engine(&self) -> &StoreEngine {
        &self.0.engine
    }
    pub fn owner(&self) -> &str {
        &self.0.owner
    }
    pub fn assert_owner(&self, owner: &str) -> Result<()> {
        if owner != self.0.owner {
            return Err(Error::Invalid(
                "job transaction belongs to another owner".into(),
            ));
        }
        Ok(())
    }
}

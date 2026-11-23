//! Artifacts in the server's Chord store: keyed documents (`p2.artifacts`, `p2.artifact-states`)
//! and the immutable `artifact_versions` table keyed by `(session, id, version)`.

use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock},
};

use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::{HostKind, read_params},
    delta::Segment,
    documents::{Definition, DocumentKey, KeyedBy},
    host::ObjectDocument,
    json::JsonValue,
    store::{ColumnRecord, ColumnType, Query, StoreEngine, StoreWrite, TableDefinition, WhereNode},
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, broadcast};
use unicode_normalization::UnicodeNormalization;

use crate::api::v1::session_address;

/// An artifact's ID: a lowercase slug, unique in its session.
pub const ARTIFACT_ID_PATTERN: &str = "^[a-z0-9][a-z0-9_-]{0,63}$";
/// Largest version content, in UTF-8 bytes (2 MiB).
pub const MAX_ARTIFACT_BYTES: usize = 2 * 1024 * 1024;
/// Largest html page state, in JSON characters (256 KiB).
pub const MAX_ARTIFACT_STATE: usize = 256 * 1024;
/// The contract name owning the `artifact_versions` table in the server store.
pub const ARTIFACT_STORE_OWNER: &str = "p2.artifact-store";
/// Table name for immutable artifact version content rows.
pub const VERSION_ROWS_TABLE: &str = "artifact_versions";

static ARTIFACT_ID_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: static regex literal verified at compile/test time.
    Regex::new(ARTIFACT_ID_PATTERN).expect("valid artifact id regex")
});

/// Returns true if `id` is a valid artifact slug.
pub fn is_valid_artifact_id(id: &str) -> bool {
    ARTIFACT_ID_REGEX.is_match(id)
}

/// The kinds of artifacts: an html page in a sandboxed frame, or a Markdown document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    Html,
    Md,
}

impl ArtifactKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Md => "md",
        }
    }
}

/// One artifact as a session's document keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredArtifact {
    pub kind: ArtifactKind,
    pub title: String,
    pub version: i64,
    pub updated: i64,
    pub created: i64,
}

/// A session's artifacts, by ID.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactsDoc {
    #[serde(default)]
    pub artifacts: BTreeMap<String, StoredArtifact>,
}

/// A session's html pages' states, by artifact ID.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StatesDoc {
    #[serde(default)]
    pub states: BTreeMap<String, Value>,
}

/// An artifact as lists show it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactSummary {
    pub id: String,
    pub kind: ArtifactKind,
    pub title: String,
    pub version: i64,
    pub updated: i64,
}

/// A version's content with its artifact's summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRead {
    pub summary: ArtifactSummary,
    pub version: i64,
    pub content: String,
}

/// What writing takes.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WriteInput {
    #[serde(default)]
    pub id: Option<String>,
    pub kind: ArtifactKind,
    pub title: String,
    pub content: String,
}

/// An exact replacement in an artifact's newest content.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArtifactEdit {
    pub old: String,
    pub new: String,
}

/// One stored artifact as lists show it.
pub fn summary_of(id: &str, stored: &StoredArtifact) -> ArtifactSummary {
    ArtifactSummary {
        id: id.to_owned(),
        kind: stored.kind,
        title: stored.title.clone(),
        version: stored.version,
        updated: stored.updated,
    }
}

/// A session's artifacts as lists show them, newest first (ties broken by ascending id).
pub fn summaries_of(doc: &ArtifactsDoc) -> Vec<ArtifactSummary> {
    let mut out: Vec<ArtifactSummary> = doc
        .artifacts
        .iter()
        .map(|(id, stored)| summary_of(id, stored))
        .collect();
    out.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
    out
}

/// Session host kind matching `Session({ id })`.
pub fn session_host_kind() -> Result<HostKind> {
    let params = read_params(
        &[(
            "id".into(),
            JsonValue::from_serde(json!({"type": "string"})),
        )],
        "session",
    )?;
    HostKind::new("session", params)
}

/// Chord document definition for `p2.artifacts` (persisted, keyed by `Session`).
pub fn artifacts_state_definition() -> Result<Arc<Definition>> {
    let schema = JsonValue::from_serde(json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["artifacts"],
        "properties": {
            "artifacts": {
                "type": "object",
                "additionalProperties": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["kind", "title", "version", "updated", "created"],
                    "properties": {
                        "kind": { "enum": ["html", "md"] },
                        "title": { "type": "string" },
                        "version": { "type": "integer", "minimum": 1 },
                        "updated": { "type": "number" },
                        "created": { "type": "number" }
                    }
                }
            }
        }
    }));
    Ok(Arc::new(
        Definition::new("p2.artifacts", schema, 1, || {
            Ok(JsonValue::from_serde(json!({ "artifacts": {} })))
        })?
        .keyed(KeyedBy::Host(session_host_kind()?))
        .persisted(true, |_, _, deltas| deltas >= 32),
    ))
}

/// Chord document definition for `p2.artifact-states` (persisted, keyed by `Session`).
pub fn states_state_definition() -> Result<Arc<Definition>> {
    let schema = JsonValue::from_serde(json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["states"],
        "properties": {
            "states": {
                "type": "object",
                "additionalProperties": {}
            }
        }
    }));
    Ok(Arc::new(
        Definition::new("p2.artifact-states", schema, 1, || {
            Ok(JsonValue::from_serde(json!({ "states": {} })))
        })?
        .keyed(KeyedBy::Host(session_host_kind()?))
        .persisted(true, |_, _, deltas| deltas >= 8),
    ))
}

/// Chord table definition for `artifact_versions`.
pub fn version_rows_table() -> Result<TableDefinition> {
    let mut columns = IndexMap::new();
    columns.insert("session".into(), ColumnRecord::host("session"));
    columns.insert("id".into(), ColumnRecord::new(ColumnType::Str));
    columns.insert("version".into(), ColumnRecord::new(ColumnType::Int));
    columns.insert("content".into(), ColumnRecord::new(ColumnType::Str));
    let table = TableDefinition::new(
        VERSION_ROWS_TABLE,
        1,
        columns,
        vec!["session".into(), "id".into(), "version".into()],
        IndexMap::new(),
    )?;
    table.bind_host("session", session_host_kind()?)
}

/// Every session's artifacts, backed by the server's Chord store and documents.
pub struct ArtifactStore {
    artifacts: ObjectDocument,
    states: ObjectDocument,
    versions: TableDefinition,
    store: StoreEngine,
    session_kind: HostKind,
    write_lock: AsyncMutex<()>,
    changed: broadcast::Sender<String>,
}

impl ArtifactStore {
    pub fn new(
        artifacts: ObjectDocument,
        states: ObjectDocument,
        versions: TableDefinition,
        store: StoreEngine,
    ) -> Result<Self> {
        let (changed, _) = broadcast::channel(256);
        Ok(Self {
            artifacts,
            states,
            versions,
            store,
            session_kind: session_host_kind()?,
            write_lock: AsyncMutex::new(()),
            changed,
        })
    }

    /// Subscribes to session change notifications (emits the changed session ID).
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.changed.subscribe()
    }

    fn session_doc_key(&self, session: &str) -> DocumentKey {
        DocumentKey::Address(JsonValue::from_serde(session_address(session)))
    }

    fn session_host_key(&self, session: &str) -> Result<String> {
        Ok(self
            .session_kind
            .address(JsonValue::from_serde(json!({ "id": session })))?
            .key)
    }

    /// Reads the current `ArtifactsDoc` for `session` (empty if never written).
    pub async fn artifacts_doc(&self, session: &str, context: &Context) -> Result<ArtifactsDoc> {
        let handle = self.artifacts.keyed_handle()?;
        let Some(rev) = handle.get(self.session_doc_key(session), context).await? else {
            return Ok(ArtifactsDoc::default());
        };
        Ok(serde_json::from_value(rev.to_json().to_serde()?)?)
    }

    /// Reads the current `StatesDoc` for `session` (empty if never written).
    pub async fn states_doc(&self, session: &str, context: &Context) -> Result<StatesDoc> {
        let handle = self.states.keyed_handle()?;
        let Some(rev) = handle.get(self.session_doc_key(session), context).await? else {
            return Ok(StatesDoc::default());
        };
        Ok(serde_json::from_value(rev.to_json().to_serde()?)?)
    }

    async fn read_version_content(
        &self,
        session: &str,
        id: &str,
        version: i64,
    ) -> Result<Option<String>> {
        let host_key = self.session_host_key(session)?;
        let query = Query::new(ARTIFACT_STORE_OWNER, self.versions.clone())?
            .filter(WhereNode::And(vec![
                WhereNode::Eq(("session".into(), Value::String(host_key))),
                WhereNode::Eq(("id".into(), Value::String(id.to_owned()))),
                WhereNode::Eq(("version".into(), json!(version))),
            ]))?
            .with_limit(1)?;
        let page = self.store.read(query).await?;
        Ok(page
            .rows
            .first()
            .and_then(|row| row.get("content"))
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    /// Version `version` (default newest) of artifact `id`'s content, with its summary.
    pub async fn read(
        &self,
        session: &str,
        id: &str,
        version: Option<i64>,
        context: &Context,
    ) -> Result<ArtifactRead> {
        let doc = self.artifacts_doc(session, context).await?;
        let Some(stored) = doc.artifacts.get(id) else {
            return Err(Error::Invalid(format!("No artifact \"{id}\"")));
        };
        let wanted = version.unwrap_or(stored.version);
        let Some(content) = self.read_version_content(session, id, wanted).await? else {
            return Err(Error::Invalid(format!(
                "Artifact \"{id}\" has no version {wanted} (its newest is {})",
                stored.version
            )));
        };
        Ok(ArtifactRead {
            summary: summary_of(id, stored),
            version: wanted,
            content,
        })
    }

    /// Writes a new artifact (or next version of an existing one of the same kind).
    pub async fn write(
        &self,
        session: &str,
        input: WriteInput,
        now: i64,
        context: &Context,
    ) -> Result<(String, i64)> {
        check_content(&input.content)?;
        if let Some(ref id) = input.id
            && !is_valid_artifact_id(id)
        {
            return Err(Error::Invalid(format!("Invalid artifact id \"{id}\"")));
        }
        let _guard = self.write_lock.lock().await;
        let current_doc = self.artifacts_doc(session, context).await?;
        let id = input
            .id
            .unwrap_or_else(|| free_id(&current_doc.artifacts, &input.title));
        let stored = current_doc.artifacts.get(&id);
        if let Some(existing) = stored
            && existing.kind != input.kind
        {
            return Err(Error::Invalid(format!(
                "Artifact \"{id}\" is {}, not {}",
                existing.kind.as_str(),
                input.kind.as_str()
            )));
        }
        let version = stored.map_or(0, |s| s.version) + 1;
        let created = stored.map_or(now, |s| s.created);
        let put = self.versions.put(
            ARTIFACT_STORE_OWNER,
            json!({
                "session": session_address(session),
                "id": id,
                "version": version,
                "content": input.content,
            }),
        )?;
        self.store.transact(move |step| step.apply(put)).await?;

        let next_entry = StoredArtifact {
            kind: input.kind,
            title: input.title,
            version,
            updated: now,
            created,
        };
        let id_clone = id.clone();
        self.artifacts
            .change_at(Some(self.session_doc_key(session)), context, move |draft| {
                Box::pin(async move {
                    let mut doc: ArtifactsDoc =
                        serde_json::from_value(draft.value()?.to_json().to_serde()?)?;
                    doc.artifacts.insert(id_clone, next_entry);
                    draft.set(
                        &[Segment::Key("artifacts".into())],
                        JsonValue::from_serde(serde_json::to_value(doc.artifacts)?),
                    )?;
                    Ok(())
                })
            })
            .await?;
        let _ = self.changed.send(session.to_owned());
        Ok((id, version))
    }

    /// Writes the next version of artifact `id` with `edits` applied to its newest content.
    pub async fn edit(
        &self,
        session: &str,
        id: &str,
        edits: &[ArtifactEdit],
        now: i64,
        context: &Context,
    ) -> Result<i64> {
        let _guard = self.write_lock.lock().await;
        let current_doc = self.artifacts_doc(session, context).await?;
        let Some(stored) = current_doc.artifacts.get(id) else {
            return Err(Error::Invalid(format!("No artifact \"{id}\"")));
        };
        let Some(current_content) = self
            .read_version_content(session, id, stored.version)
            .await?
        else {
            return Err(Error::Invalid(format!(
                "Artifact \"{id}\" lost its version {}",
                stored.version
            )));
        };
        let content = apply_edits(&current_content, edits)?;
        check_content(&content)?;
        let version = stored.version + 1;
        let put = self.versions.put(
            ARTIFACT_STORE_OWNER,
            json!({
                "session": session_address(session),
                "id": id,
                "version": version,
                "content": content,
            }),
        )?;
        self.store.transact(move |step| step.apply(put)).await?;

        let id_owned = id.to_owned();
        self.artifacts
            .change_at(Some(self.session_doc_key(session)), context, move |draft| {
                Box::pin(async move {
                    let mut doc: ArtifactsDoc =
                        serde_json::from_value(draft.value()?.to_json().to_serde()?)?;
                    if let Some(entry) = doc.artifacts.get_mut(&id_owned) {
                        entry.version = version;
                        entry.updated = now;
                    }
                    draft.set(
                        &[Segment::Key("artifacts".into())],
                        JsonValue::from_serde(serde_json::to_value(doc.artifacts)?),
                    )?;
                    Ok(())
                })
            })
            .await?;
        let _ = self.changed.send(session.to_owned());
        Ok(version)
    }

    /// Removes artifact `id` with all its versions and state.
    pub async fn remove(&self, session: &str, id: &str, context: &Context) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let current_doc = self.artifacts_doc(session, context).await?;
        let Some(stored) = current_doc.artifacts.get(id) else {
            return Err(Error::Invalid(format!("No artifact \"{id}\"")));
        };
        let max_version = stored.version;
        let host_key = self.session_host_key(session)?;
        let table = format!("{ARTIFACT_STORE_OWNER}:{VERSION_ROWS_TABLE}");
        let mut deletes = Vec::new();
        for version in 1..=max_version {
            let mut key = IndexMap::new();
            key.insert("session".into(), Value::String(host_key.clone()));
            key.insert("id".into(), Value::String(id.to_owned()));
            key.insert("version".into(), json!(version));
            deletes.push(StoreWrite::Delete {
                table: table.clone(),
                key,
                count: None,
            });
        }
        if !deletes.is_empty() {
            self.store
                .transact(move |step| {
                    for write in deletes {
                        step.apply(write)?;
                    }
                    Ok(())
                })
                .await?;
        }
        let id_owned = id.to_owned();
        self.artifacts
            .change_at(Some(self.session_doc_key(session)), context, move |draft| {
                Box::pin(async move {
                    let mut doc: ArtifactsDoc =
                        serde_json::from_value(draft.value()?.to_json().to_serde()?)?;
                    doc.artifacts.remove(&id_owned);
                    draft.set(
                        &[Segment::Key("artifacts".into())],
                        JsonValue::from_serde(serde_json::to_value(doc.artifacts)?),
                    )?;
                    Ok(())
                })
            })
            .await?;
        let id_owned = id.to_owned();
        self.states
            .change_at(Some(self.session_doc_key(session)), context, move |draft| {
                Box::pin(async move {
                    let mut doc: StatesDoc =
                        serde_json::from_value(draft.value()?.to_json().to_serde()?)?;
                    doc.states.remove(&id_owned);
                    draft.set(
                        &[Segment::Key("states".into())],
                        JsonValue::from_serde(serde_json::to_value(doc.states)?),
                    )?;
                    Ok(())
                })
            })
            .await?;
        let _ = self.changed.send(session.to_owned());
        Ok(())
    }

    /// Copies session `from`'s artifacts, every version, and states to session `to`.
    pub async fn copy(&self, from: &str, to: &str, context: &Context) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let source_artifacts = self.artifacts_doc(from, context).await?;
        let source_states = self.states_doc(from, context).await?;
        let mut puts = Vec::new();
        for (id, stored) in &source_artifacts.artifacts {
            for version in 1..=stored.version {
                if let Some(content) = self.read_version_content(from, id, version).await? {
                    puts.push(self.versions.put(
                        ARTIFACT_STORE_OWNER,
                        json!({
                            "session": session_address(to),
                            "id": id,
                            "version": version,
                            "content": content,
                        }),
                    )?);
                }
            }
        }
        if !puts.is_empty() {
            self.store
                .transact(move |step| {
                    for write in puts {
                        step.apply(write)?;
                    }
                    Ok(())
                })
                .await?;
        }
        let artifacts_copy = source_artifacts.artifacts;
        self.artifacts
            .change_at(Some(self.session_doc_key(to)), context, move |draft| {
                Box::pin(async move {
                    let mut doc: ArtifactsDoc =
                        serde_json::from_value(draft.value()?.to_json().to_serde()?)?;
                    for (id, stored) in artifacts_copy {
                        doc.artifacts.insert(id, stored);
                    }
                    draft.set(
                        &[Segment::Key("artifacts".into())],
                        JsonValue::from_serde(serde_json::to_value(doc.artifacts)?),
                    )?;
                    Ok(())
                })
            })
            .await?;
        let states_copy = source_states.states;
        self.states
            .change_at(Some(self.session_doc_key(to)), context, move |draft| {
                Box::pin(async move {
                    let mut doc: StatesDoc =
                        serde_json::from_value(draft.value()?.to_json().to_serde()?)?;
                    for (id, state) in states_copy {
                        doc.states.insert(id, state);
                    }
                    draft.set(
                        &[Segment::Key("states".into())],
                        JsonValue::from_serde(serde_json::to_value(doc.states)?),
                    )?;
                    Ok(())
                })
            })
            .await?;
        let _ = self.changed.send(to.to_owned());
        Ok(())
    }

    /// Html artifact `id`'s state (`Value::Null` until something set it).
    pub async fn state(&self, session: &str, id: &str, context: &Context) -> Result<Value> {
        let doc = self.artifacts_doc(session, context).await?;
        let Some(stored) = doc.artifacts.get(id) else {
            return Err(Error::Invalid(format!("No artifact \"{id}\"")));
        };
        if stored.kind != ArtifactKind::Html {
            return Err(Error::Invalid(format!(
                "Artifact \"{id}\" is {}: only html pages have a state",
                stored.kind.as_str()
            )));
        }
        let states = self.states_doc(session, context).await?;
        Ok(states.states.get(id).cloned().unwrap_or(Value::Null))
    }

    /// Sets html artifact `id`'s state (any JSON, at most `MAX_ARTIFACT_STATE` chars).
    pub async fn set_state(
        &self,
        session: &str,
        id: &str,
        state: Value,
        context: &Context,
    ) -> Result<()> {
        let serialized = serde_json::to_string(&state)?;
        let size = serialized.encode_utf16().count();
        if size > MAX_ARTIFACT_STATE {
            return Err(Error::Invalid(format!(
                "The state is {size} characters of JSON; at most {MAX_ARTIFACT_STATE}"
            )));
        }
        let _guard = self.write_lock.lock().await;
        let doc = self.artifacts_doc(session, context).await?;
        let Some(stored) = doc.artifacts.get(id) else {
            return Err(Error::Invalid(format!("No artifact \"{id}\"")));
        };
        if stored.kind != ArtifactKind::Html {
            return Err(Error::Invalid(format!(
                "Artifact \"{id}\" is {}: only html pages have a state",
                stored.kind.as_str()
            )));
        }
        let id_owned = id.to_owned();
        self.states
            .change_at(Some(self.session_doc_key(session)), context, move |draft| {
                Box::pin(async move {
                    let mut doc: StatesDoc =
                        serde_json::from_value(draft.value()?.to_json().to_serde()?)?;
                    doc.states.insert(id_owned, state);
                    draft.set(
                        &[Segment::Key("states".into())],
                        JsonValue::from_serde(serde_json::to_value(doc.states)?),
                    )?;
                    Ok(())
                })
            })
            .await?;
        let _ = self.changed.send(session.to_owned());
        Ok(())
    }
}

/// A free ID made from `title`: its slug, numbered when taken.
pub fn free_id<V>(taken: &BTreeMap<String, V>, title: &str) -> String {
    let normalized: String = title.to_lowercase().nfkd().collect();
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in normalized.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            slug.push(ch);
            prev_dash = false;
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-');
    let shortened: String = trimmed.chars().take(48).collect();
    let candidate = if shortened.is_empty() {
        "artifact".to_owned()
    } else {
        shortened
    };
    let base = if is_valid_artifact_id(&candidate) {
        candidate
    } else {
        format!("a-{candidate}").chars().take(48).collect()
    };
    if !taken.contains_key(&base) {
        return base;
    }
    for n in 2usize.. {
        let next = format!("{base}-{n}");
        if !taken.contains_key(&next) {
            return next;
        }
    }
    base
}

fn check_content(content: &str) -> Result<()> {
    if content.len() > MAX_ARTIFACT_BYTES {
        return Err(Error::Invalid("The content exceeds 2 MiB".into()));
    }
    Ok(())
}

/// Applies `edits` to `text`: each `old` must occur exactly once, none may overlap; all or nothing.
pub fn apply_edits(text: &str, edits: &[ArtifactEdit]) -> Result<String> {
    struct Range<'a> {
        start: usize,
        end: usize,
        replacement: &'a str,
    }
    let mut ranges = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        if edit.old.is_empty() {
            return Err(Error::Invalid(format!(
                "edits[{index}]: old text must not be empty"
            )));
        }
        let Some(at) = text.find(&edit.old) else {
            return Err(Error::Invalid(format!(
                "edits[{index}]: old text not found"
            )));
        };
        let search_after = at + text[at..].chars().next().map_or(1, char::len_utf8);
        if search_after <= text.len() && text[search_after..].contains(&edit.old) {
            return Err(Error::Invalid(format!(
                "edits[{index}]: old text occurs more than once; include more context"
            )));
        }
        ranges.push(Range {
            start: at,
            end: at + edit.old.len(),
            replacement: &edit.new,
        });
    }
    ranges.sort_by_key(|r| r.start);
    for pair in ranges.windows(2) {
        if pair[1].start < pair[0].end {
            return Err(Error::Invalid(
                "Two edits overlap; merge them into one".into(),
            ));
        }
    }
    let mut next = text.to_owned();
    for range in ranges.iter().rev() {
        next.replace_range(range.start..range.end, range.replacement);
    }
    Ok(next)
}

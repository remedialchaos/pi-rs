//! Versioned logical files, immutable blobs, resumable uploads and live folder views.
use super::{new_token, private_directory, safe_relative};
use crate::{
    Error, Result,
    context::Context,
    json::JsonValue,
    state::Source,
    store::{
        ColumnRecord, ColumnType, JsonColumn, Query, StoreEngine, StoreWrite, TableDefinition,
    },
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
mod facet;
pub use facet::{files_contract, files_folder_contract};
const OWNER: &str = "chord.app.files";
const CHUNK: usize = 4 * 1024 * 1024;
const FOLDER_VIEW: usize = 500;
const UPLOAD_IDLE: Duration = Duration::from_secs(3600);
const UPLOAD_SWEEP: Duration = Duration::from_secs(60);
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FileInfo {
    pub id: String,
    pub space: String,
    pub path: String,
    pub version: u64,
    pub size: u64,
    pub mime: String,
    pub source: String,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub created: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileWrite {
    pub space: String,
    pub path: String,
    pub mime: String,
    pub source: String,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    pub key: Option<String>,
    pub created: Option<u64>,
    pub meta: Option<Value>,
}
impl FileWrite {
    pub fn new(space: &str, path: &str) -> Self {
        Self {
            space: space.into(),
            path: path.into(),
            mime: "application/octet-stream".into(),
            source: String::new(),
            reference: None,
            key: None,
            created: None,
            meta: None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FolderInfo {
    pub space: String,
    pub path: String,
    pub created: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FolderEntry {
    Folder { folder: FolderInfo },
    File { file: Box<FileInfo> },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SpaceSummary {
    pub files: usize,
    pub versions: usize,
    pub bytes: u64,
    pub newest: Vec<FileInfo>,
}
#[derive(Clone, Copy, Debug)]
pub struct FilesCaps {
    pub max_file: u64,
    pub max_total: u64,
    pub max_uploads: usize,
}
impl Default for FilesCaps {
    fn default() -> Self {
        Self {
            max_file: 256 * 1024 * 1024,
            max_total: 4 * 1024 * 1024 * 1024,
            max_uploads: 64,
        }
    }
}
#[derive(Clone)]
struct Record {
    info: FileInfo,
    blob: String,
}
#[derive(Clone, Default)]
struct Index {
    files: BTreeMap<String, Record>,
    folders: BTreeMap<(String, String), FolderInfo>,
    marks: BTreeMap<(String, String), u64>,
}
struct Upload {
    write: FileWrite,
    path: PathBuf,
    size: u64,
    touched: tokio::time::Instant,
}
struct Inner {
    store: StoreEngine,
    dir: PathBuf,
    caps: FilesCaps,
    tables: Vec<TableDefinition>,
    index: Mutex<Index>,
    line: tokio::sync::Mutex<()>,
    uploads: Mutex<BTreeMap<String, Upload>>,
    views: Mutex<BTreeMap<String, (String, String, Source)>>,
    summary: Source,
    sweep: Mutex<Option<tokio::task::AbortHandle>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(task) = self
            .sweep
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            task.abort();
        }
        for upload in self
            .uploads
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            let _ = std::fs::remove_file(&upload.path);
        }
    }
}
#[derive(Clone)]
pub struct Files(Arc<Inner>);
fn check_space(space: &str) -> Result<()> {
    if space.is_empty()
        || space.len() > 64
        || !space.as_bytes()[0].is_ascii_lowercase() && !space.as_bytes()[0].is_ascii_digit()
        || !space
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    {
        return Err(Error::Invalid("invalid file space".into()));
    }
    Ok(())
}
fn check_path(path: &str) -> Result<()> {
    if path.encode_utf16().count() > 1024
        || path.contains('\0')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(Error::Invalid("invalid logical file path".into()));
    }
    Ok(())
}
fn check_write(write: &FileWrite) -> Result<()> {
    check_space(&write.space)?;
    check_path(&write.path)?;
    if write.key.as_ref().is_some_and(String::is_empty)
        || write.reference.as_ref().is_some_and(String::is_empty)
    {
        return Err(Error::Invalid("invalid file metadata".into()));
    }
    Ok(())
}
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(folder, _)| folder)
}
fn check_destination(index: &Index, write: &FileWrite) -> Result<()> {
    if index
        .folders
        .contains_key(&(write.space.clone(), write.path.clone()))
    {
        return Err(Error::Invalid("file path is a folder".into()));
    }
    let mut folder = parent(&write.path);
    while !folder.is_empty() {
        if index
            .files
            .values()
            .any(|r| r.info.space == write.space && r.info.path == folder)
        {
            return Err(Error::Invalid("parent path is a file".into()));
        }
        folder = parent(folder);
    }
    Ok(())
}
fn definitions() -> Result<Vec<TableDefinition>> {
    let mut columns = IndexMap::new();
    for name in ["id", "space", "folder", "path"] {
        columns.insert(name.into(), ColumnRecord::new(ColumnType::Str));
    }
    for name in ["version", "latest"] {
        columns.insert(name.into(), ColumnRecord::new(ColumnType::Int));
    }
    columns.insert("blob".into(), ColumnRecord::new(ColumnType::Str));
    columns.insert("size".into(), ColumnRecord::new(ColumnType::Int));
    for name in ["mime", "source", "ref", "key"] {
        columns.insert(name.into(), ColumnRecord::new(ColumnType::Str));
    }
    columns.insert("created".into(), ColumnRecord::new(ColumnType::Int));
    let meta = JsonColumn::new(crate::contracts::schema::unknown(), 1, None)?;
    columns.insert("meta".into(), meta.record().optional());
    let indexes = IndexMap::from([
        (
            "by_folder".into(),
            vec![
                "space".into(),
                "folder".into(),
                "latest".into(),
                "path".into(),
            ],
        ),
        (
            "by_path".into(),
            vec!["space".into(), "path".into(), "version".into()],
        ),
        (
            "by_ref".into(),
            vec!["space".into(), "ref".into(), "created".into()],
        ),
        ("by_key".into(), vec!["space".into(), "key".into()]),
        ("by_created".into(), vec!["space".into(), "created".into()]),
    ]);
    let files = TableDefinition::new("files", 1, columns, vec!["id".into()], indexes)?
        .bind_json("meta", meta)?;
    let folders = TableDefinition::new(
        "folders",
        1,
        IndexMap::from([
            ("id".into(), ColumnRecord::new(ColumnType::Str)),
            ("space".into(), ColumnRecord::new(ColumnType::Str)),
            ("parent".into(), ColumnRecord::new(ColumnType::Str)),
            ("path".into(), ColumnRecord::new(ColumnType::Str)),
            ("created".into(), ColumnRecord::new(ColumnType::Int)),
        ]),
        vec!["id".into()],
        IndexMap::from([
            ("by_path".into(), vec!["space".into(), "path".into()]),
            (
                "by_parent".into(),
                vec!["space".into(), "parent".into(), "path".into()],
            ),
        ]),
    )?;
    let marks = TableDefinition::new(
        "paths",
        1,
        IndexMap::from([
            ("space".into(), ColumnRecord::new(ColumnType::Str)),
            ("path".into(), ColumnRecord::new(ColumnType::Str)),
            ("last".into(), ColumnRecord::new(ColumnType::Int)),
        ]),
        vec!["space".into(), "path".into()],
        IndexMap::new(),
    )?;
    let blobs = TableDefinition::new(
        "blobs",
        1,
        IndexMap::from([
            ("id".into(), ColumnRecord::new(ColumnType::Str)),
            ("size".into(), ColumnRecord::new(ColumnType::Int)),
            ("refs".into(), ColumnRecord::new(ColumnType::Int)),
        ]),
        vec!["id".into()],
        IndexMap::new(),
    )?;
    Ok(vec![files, folders, marks, blobs])
}
impl Files {
    pub async fn open(store: StoreEngine, dir: PathBuf, caps: FilesCaps) -> Result<Self> {
        private_directory(&dir)?;
        private_directory(&dir.join("blobs"))?;
        private_directory(&dir.join(".uploads"))?;
        let tables = definitions()?;
        let mut writes = Vec::new();
        for table in &tables {
            let recorded =
                crate::store::recorded_table(store.database(), format!("{OWNER}:{}", table.name))
                    .await?;
            if let Some(recorded) = recorded {
                let recorded: crate::store::TableRecord = serde_json::from_value(recorded)?;
                if recorded.version != table.version
                    || recorded.fingerprint != table.fingerprint
                    || recorded.columns != table.columns
                    || recorded.key != table.key
                    || recorded.indexes != table.indexes
                    || recorded.state != crate::store::TableState::Ready
                {
                    return Err(Error::Invalid("stored Files schema differs".into()));
                }
            } else {
                writes.extend(table.declaration(OWNER)?);
            }
        }
        if !writes.is_empty() {
            store.commit(writes).await?;
        }
        let mut index = Index::default();
        for table in &tables[..3] {
            let mut cursor = None;
            loop {
                let page = store
                    .read(
                        Query::new(OWNER, table.clone())?
                            .with_limit(1000)?
                            .after(cursor.as_deref())?,
                    )
                    .await?;
                for row in page.rows {
                    match table.name.as_str() {
                        "files" => {
                            let blob = row
                                .get("blob")
                                .and_then(Value::as_str)
                                .ok_or_else(|| Error::Invalid("file blob absent".into()))?
                                .to_string();
                            safe_relative(&blob)?;
                            if blob.contains('/') {
                                return Err(Error::Invalid("invalid blob id".into()));
                            }
                            let mut info: FileInfo = serde_json::from_value(row)?;
                            info.reference = info.reference.filter(|s| !s.is_empty());
                            info.key = info.key.filter(|s| !s.is_empty());
                            check_write(&FileWrite {
                                space: info.space.clone(),
                                path: info.path.clone(),
                                mime: info.mime.clone(),
                                source: info.source.clone(),
                                reference: info.reference.clone(),
                                key: info.key.clone(),
                                created: Some(info.created),
                                meta: info.meta.clone(),
                            })?;
                            index.files.insert(info.id.clone(), Record { info, blob });
                        }
                        "folders" => {
                            let folder: FolderInfo = serde_json::from_value(row)?;
                            index
                                .folders
                                .insert((folder.space.clone(), folder.path.clone()), folder);
                        }
                        _ => {
                            let space = row
                                .get("space")
                                .and_then(Value::as_str)
                                .ok_or_else(|| Error::Invalid("path space absent".into()))?
                                .into();
                            let path = row
                                .get("path")
                                .and_then(Value::as_str)
                                .ok_or_else(|| Error::Invalid("path absent".into()))?
                                .into();
                            let last = row
                                .get("last")
                                .and_then(Value::as_u64)
                                .ok_or_else(|| Error::Invalid("path last absent".into()))?;
                            index.marks.insert((space, path), last);
                        }
                    }
                }
                cursor = page.cursor;
                if cursor.is_none() {
                    break;
                }
            }
        }
        // Leftover uncommitted uploads and orphan blobs cannot become visible after a restart.
        for entry in std::fs::read_dir(dir.join(".uploads"))? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                std::fs::remove_file(entry.path())?;
            }
        }
        for entry in std::fs::read_dir(dir.join("blobs"))? {
            let entry = entry?;
            if !index
                .files
                .values()
                .any(|r| r.blob == entry.file_name().to_string_lossy())
            {
                std::fs::remove_file(entry.path())?;
            }
        }
        let summary = Source::new(store.batch(), Self::summaries(&index)?)?;
        let files = Self(Arc::new(Inner {
            store,
            dir,
            caps,
            tables,
            index: Mutex::new(index),
            line: tokio::sync::Mutex::new(()),
            uploads: Mutex::new(BTreeMap::new()),
            views: Mutex::new(BTreeMap::new()),
            summary,
            sweep: Mutex::new(None),
        }));
        let weak = Arc::downgrade(&files.0);
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + UPLOAD_SWEEP, UPLOAD_SWEEP);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let sweep = tokio::spawn(async move {
            loop {
                interval.tick().await;
                let Some(inner) = weak.upgrade() else { break };
                let _ = Self(inner).expire_uploads(UPLOAD_IDLE.as_secs());
            }
        });
        *files.0.sweep.lock().unwrap_or_else(|e| e.into_inner()) = Some(sweep.abort_handle());
        Ok(files)
    }
    pub fn directory(&self) -> &Path {
        &self.0.dir
    }
    pub fn get(&self, space: &str, path: &str, version: Option<u64>) -> Result<Option<FileInfo>> {
        check_space(space)?;
        check_path(path)?;
        Ok(self
            .0
            .index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .files
            .values()
            .filter(|r| {
                r.info.space == space
                    && r.info.path == path
                    && version.is_none_or(|v| v == r.info.version)
            })
            .max_by_key(|r| r.info.version)
            .map(|r| r.info.clone()))
    }
    pub fn file(&self, id: &str) -> Option<FileInfo> {
        self.0
            .index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .files
            .get(id)
            .map(|r| r.info.clone())
    }
    pub fn by_key(&self, space: &str, key: &str) -> Option<FileInfo> {
        self.0
            .index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .files
            .values()
            .find(|r| r.info.space == space && r.info.key.as_deref() == Some(key))
            .map(|r| r.info.clone())
    }
    pub fn versions(&self, space: &str, path: &str) -> Result<Vec<FileInfo>> {
        check_space(space)?;
        check_path(path)?;
        let mut versions = self
            .0
            .index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .files
            .values()
            .filter(|r| r.info.space == space && r.info.path == path)
            .map(|r| r.info.clone())
            .collect::<Vec<_>>();
        versions.sort_by_key(|f| std::cmp::Reverse(f.version));
        Ok(versions)
    }
    pub fn read(&self, id: &str, offset: u64, length: usize) -> Result<Vec<u8>> {
        if length > CHUNK {
            return Err(Error::Invalid("read exceeds 4 MiB".into()));
        }
        let index = self.0.index.lock().unwrap_or_else(|e| e.into_inner());
        let record = index
            .files
            .get(id)
            .ok_or_else(|| Error::Invalid("file not found".into()))?;
        let path = self.0.dir.join("blobs").join(&record.blob);
        if std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(Error::Invalid("blob is a symlink".into()));
        }
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.take((length as u64).min(record.info.size.saturating_sub(offset)))
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    fn snapshot(&self) -> Index {
        self.0
            .index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    async fn commit(&self, before: &Index, next: Index) -> Result<()> {
        let mut writes = Vec::new();
        let mut blobs = BTreeMap::<String, (u64, u64)>::new();
        let mut old_blobs = BTreeMap::new();
        for record in before.files.values() {
            old_blobs.insert(record.blob.clone(), ());
        }
        for record in next.files.values() {
            let mut row = serde_json::to_value(&record.info)?;
            let obj = row
                .as_object_mut()
                .ok_or_else(|| Error::Invalid("file info not object".into()))?;
            obj.insert("folder".into(), json!(parent(&record.info.path)));
            obj.insert("blob".into(), json!(record.blob));
            let latest = !next.files.values().any(|r| {
                r.info.space == record.info.space
                    && r.info.path == record.info.path
                    && r.info.version > record.info.version
            });
            obj.insert("latest".into(), json!(u8::from(latest)));
            obj.insert(
                "ref".into(),
                json!(record.info.reference.as_deref().unwrap_or("")),
            );
            obj.insert(
                "key".into(),
                json!(record.info.key.as_deref().unwrap_or("")),
            );
            writes.push(self.0.tables[0].put(OWNER, row)?);
            let blob = blobs
                .entry(record.blob.clone())
                .or_insert((record.info.size, 0));
            blob.1 += 1;
        }
        for id in before
            .files
            .keys()
            .filter(|id| !next.files.contains_key(*id))
        {
            writes.push(StoreWrite::Delete {
                table: format!("{OWNER}:files"),
                key: IndexMap::from([("id".into(), json!(id))]),
                count: None,
            });
        }
        for folder in next.folders.values() {
            writes.push(self.0.tables[1].put(OWNER,json!({"id":format!("{}|{}",folder.space,folder.path),"space":folder.space,"parent":parent(&folder.path),"path":folder.path,"created":folder.created}))?);
        }
        for (space, path) in before
            .folders
            .keys()
            .filter(|key| !next.folders.contains_key(*key))
        {
            writes.push(StoreWrite::Delete {
                table: format!("{OWNER}:folders"),
                key: IndexMap::from([("id".into(), json!(format!("{space}|{path}")))]),
                count: None,
            });
        }
        for ((space, path), last) in &next.marks {
            writes
                .push(self.0.tables[2].put(OWNER, json!({"space":space,"path":path,"last":last}))?);
        }
        for (id, (size, refs)) in &blobs {
            writes.push(self.0.tables[3].put(OWNER, json!({"id":id,"size":size,"refs":refs}))?);
        }
        for id in old_blobs.keys().filter(|id| !blobs.contains_key(*id)) {
            writes.push(StoreWrite::Delete {
                table: format!("{OWNER}:blobs"),
                key: IndexMap::from([("id".into(), json!(id))]),
                count: None,
            });
        }
        self.0.store.commit(writes).await?;
        let summary = Self::summaries(&next)?;
        *self.0.index.lock().unwrap_or_else(|e| e.into_inner()) = next;
        let context = Context::background();
        self.0.summary.set(summary, &context)?;
        for id in old_blobs.keys().filter(|id| !blobs.contains_key(*id)) {
            match std::fs::remove_file(self.0.dir.join("blobs").join(id)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let views = self
            .0
            .views
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for (space, folder, source) in views {
            source.set(self.listing(&space, &folder)?, &context)?;
        }
        Ok(())
    }
    fn add(
        &self,
        index: &mut Index,
        write: FileWrite,
        blob: String,
        size: u64,
    ) -> Result<FileInfo> {
        check_write(&write)?;
        check_destination(index, &write)?;
        let version = index
            .marks
            .get(&(write.space.clone(), write.path.clone()))
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| Error::Invalid("file version overflow".into()))?;
        let info = FileInfo {
            id: new_token(18),
            space: write.space,
            path: write.path,
            version,
            size,
            mime: write.mime,
            source: write.source,
            reference: write.reference,
            key: write.key,
            created: write
                .created
                .unwrap_or(crate::node::http::now_seconds() * 1000),
            meta: write.meta,
        };
        let mut folder = parent(&info.path);
        while !folder.is_empty() {
            index
                .folders
                .entry((info.space.clone(), folder.into()))
                .or_insert_with(|| FolderInfo {
                    space: info.space.clone(),
                    path: folder.into(),
                    created: info.created,
                });
            folder = parent(folder);
        }
        index
            .marks
            .insert((info.space.clone(), info.path.clone()), version);
        index.files.insert(
            info.id.clone(),
            Record {
                info: info.clone(),
                blob,
            },
        );
        Ok(info)
    }
    pub async fn put(&self, write: FileWrite, bytes: &[u8]) -> Result<FileInfo> {
        check_write(&write)?;
        if bytes.len() > CHUNK {
            return Err(Error::Invalid("put exceeds 4 MiB".into()));
        }
        let _line = self.0.line.lock().await;
        if let Some(existing) = write
            .key
            .as_deref()
            .and_then(|key| self.by_key(&write.space, key))
        {
            return Ok(existing);
        }
        let before = self.snapshot();
        let mut next = before.clone();
        self.check_capacity(bytes.len() as u64)?;
        check_destination(&before, &write)?;
        let blob = new_token(18);
        let staging = self.0.dir.join(".uploads").join(&blob);
        write_private(&staging, bytes)?;
        std::fs::rename(&staging, self.0.dir.join("blobs").join(&blob))?;
        let info = self.add(&mut next, write, blob.clone(), bytes.len() as u64)?;
        if let Err(error) = self.commit(&before, next).await {
            if self.file(&info.id).is_none() {
                let _ = std::fs::remove_file(self.0.dir.join("blobs").join(blob));
            }
            return Err(error);
        }
        Ok(info)
    }
    fn check_capacity(&self, additional: u64) -> Result<()> {
        let index = self.snapshot();
        let mut blobs = BTreeMap::new();
        for record in index.files.values() {
            blobs.insert(&record.blob, record.info.size);
        }
        let uploads = self
            .0
            .uploads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|u| u.size)
            .sum::<u64>();
        if additional > self.0.caps.max_file
            || blobs
                .values()
                .sum::<u64>()
                .saturating_add(uploads)
                .saturating_add(additional)
                > self.0.caps.max_total
        {
            return Err(Error::Invalid("files capacity exceeded".into()));
        }
        Ok(())
    }
    pub fn list(&self, space: &str, folder: &str) -> Result<Vec<FolderEntry>> {
        check_space(space)?;
        if !folder.is_empty() {
            check_path(folder)?;
        }
        let index = self.0.index.lock().unwrap_or_else(|e| e.into_inner());
        let mut entries = index
            .folders
            .values()
            .filter(|f| f.space == space && parent(&f.path) == folder)
            .map(|f| FolderEntry::Folder { folder: f.clone() })
            .collect::<Vec<_>>();
        let mut files = BTreeMap::<String, FileInfo>::new();
        for record in index
            .files
            .values()
            .filter(|r| r.info.space == space && parent(&r.info.path) == folder)
        {
            let info = &record.info;
            if files
                .get(&info.path)
                .is_none_or(|old| old.version < info.version)
            {
                files.insert(info.path.clone(), info.clone());
            }
        }
        entries.extend(files.into_values().map(|file| FolderEntry::File {
            file: Box::new(file),
        }));
        Ok(entries)
    }
    pub fn summary(&self, space: &str) -> Result<SpaceSummary> {
        check_space(space)?;
        let index = self.0.index.lock().unwrap_or_else(|e| e.into_inner());
        Ok(Self::summarize(
            index
                .files
                .values()
                .filter(|r| r.info.space == space)
                .map(|r| &r.info)
                .collect(),
        ))
    }
    fn summaries(index: &Index) -> Result<JsonValue> {
        let mut spaces = BTreeMap::<&str, Vec<&FileInfo>>::new();
        for record in index.files.values() {
            spaces
                .entry(&record.info.space)
                .or_default()
                .push(&record.info);
        }
        let summaries = spaces
            .into_iter()
            .map(|(space, files)| (space, Self::summarize(files)))
            .collect::<BTreeMap<_, _>>();
        Ok(JsonValue::from_serde(serde_json::to_value(summaries)?))
    }
    fn summarize(mut newest: Vec<&FileInfo>) -> SpaceSummary {
        let versions = newest.len();
        let bytes = newest.iter().map(|f| f.size).sum();
        let files = newest
            .iter()
            .map(|f| &f.path)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        newest.sort_by_key(|f| std::cmp::Reverse(f.created));
        newest.truncate(20);
        SpaceSummary {
            files,
            versions,
            bytes,
            newest: newest.into_iter().cloned().collect(),
        }
    }
    pub async fn mkdir(&self, space: &str, path: &str) -> Result<()> {
        self.make_folder(space, path).await.map(drop)
    }
    async fn make_folder(&self, space: &str, path: &str) -> Result<FolderInfo> {
        check_space(space)?;
        check_path(path)?;
        let _line = self.0.line.lock().await;
        let before = self.snapshot();
        let mut next = before.clone();
        let mut folder = path;
        while !folder.is_empty() {
            if before
                .files
                .values()
                .any(|r| r.info.space == space && r.info.path == folder)
            {
                return Err(Error::Invalid("folder conflicts with a file".into()));
            }
            next.folders
                .entry((space.into(), folder.into()))
                .or_insert_with(|| FolderInfo {
                    space: space.into(),
                    path: folder.into(),
                    created: crate::node::http::now_seconds() * 1000,
                });
            folder = parent(folder);
        }
        let info = next
            .folders
            .get(&(space.into(), path.into()))
            .cloned()
            .ok_or_else(|| Error::Invalid("folder not found".into()))?;
        self.commit(&before, next).await?;
        Ok(info)
    }
    pub async fn copy(&self, id: &str, write: FileWrite) -> Result<FileInfo> {
        check_write(&write)?;
        let _line = self.0.line.lock().await;
        if let Some(info) = write
            .key
            .as_deref()
            .and_then(|key| self.by_key(&write.space, key))
        {
            return Ok(info);
        }
        let before = self.snapshot();
        let record = before
            .files
            .get(id)
            .ok_or_else(|| Error::Invalid("file not found".into()))?;
        let mut next = before.clone();
        let info = self.add(&mut next, write, record.blob.clone(), record.info.size)?;
        self.commit(&before, next).await?;
        Ok(info)
    }
    pub async fn move_path(&self, space: &str, from: &str, to: &str) -> Result<()> {
        check_space(space)?;
        check_path(from)?;
        check_path(to)?;
        if to == from || to.starts_with(&format!("{from}/")) {
            return Err(Error::Invalid("cannot move folder into itself".into()));
        }
        let _line = self.0.line.lock().await;
        let before = self.snapshot();
        if !before.files.values().any(|r| {
            r.info.space == space
                && (r.info.path == from || r.info.path.starts_with(&format!("{from}/")))
        }) && !before.folders.contains_key(&(space.into(), from.into()))
        {
            return Err(Error::Invalid("move source not found".into()));
        }
        if before.folders.contains_key(&(space.into(), to.into())) {
            return Err(Error::Invalid("destination exists".into()));
        }
        if before.files.values().any(|r| {
            r.info.space == space
                && (r.info.path == to || r.info.path.starts_with(&format!("{to}/")))
        }) {
            return Err(Error::Invalid("destination exists".into()));
        }
        let mut next = before.clone();
        for record in next.files.values_mut().filter(|r| {
            r.info.space == space
                && (r.info.path == from || r.info.path.starts_with(&format!("{from}/")))
        }) {
            record.info.path = format!("{to}{}", &record.info.path[from.len()..]);
            next.marks
                .entry((space.into(), record.info.path.clone()))
                .and_modify(|v| *v = (*v).max(record.info.version))
                .or_insert(record.info.version);
        }
        for folder in before.folders.values().filter(|f| {
            f.space == space && (f.path == from || f.path.starts_with(&format!("{from}/")))
        }) {
            next.folders.remove(&(space.into(), folder.path.clone()));
            let mut folder = folder.clone();
            folder.path = format!("{to}{}", &folder.path[from.len()..]);
            next.folders
                .insert((space.into(), folder.path.clone()), folder);
        }
        self.commit(&before, next).await
    }
    pub async fn remove(&self, space: &str, path: &str, version: Option<u64>) -> Result<()> {
        check_space(space)?;
        check_path(path)?;
        let _line = self.0.line.lock().await;
        let before = self.snapshot();
        let mut next = before.clone();
        next.files.retain(|_, r| {
            !(r.info.space == space
                && (r.info.path == path
                    || version.is_none() && r.info.path.starts_with(&format!("{path}/")))
                && version.is_none_or(|v| r.info.version == v))
        });
        if version.is_none() {
            next.folders
                .retain(|(s, p), _| s != space || p != path && !p.starts_with(&format!("{path}/")));
        }
        self.commit(&before, next).await
    }
    async fn remove_id(&self, id: &str) -> Result<()> {
        let _line = self.0.line.lock().await;
        let before = self.snapshot();
        let mut next = before.clone();
        if next.files.remove(id).is_some() {
            self.commit(&before, next).await?;
        }
        Ok(())
    }
    async fn remove_space(&self, space: &str) -> Result<()> {
        check_space(space)?;
        let _line = self.0.line.lock().await;
        let before = self.snapshot();
        let mut next = before.clone();
        next.files.retain(|_, record| record.info.space != space);
        next.folders.retain(|(s, _), _| s != space);
        self.commit(&before, next).await
    }
    pub fn begin(&self, write: FileWrite) -> Result<String> {
        check_write(&write)?;
        let mut uploads = self.0.uploads.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((id, _)) = uploads.iter().find(|(_, u)| {
            write.key.is_some() && u.write.space == write.space && u.write.key == write.key
        }) {
            return Ok(id.clone());
        }
        if uploads.len() >= self.0.caps.max_uploads {
            return Err(Error::Invalid("too many uploads".into()));
        }
        let id = new_token(18);
        let path = self.0.dir.join(".uploads").join(&id);
        write_private(&path, &[])?;
        uploads.insert(
            id.clone(),
            Upload {
                write,
                path,
                size: 0,
                touched: tokio::time::Instant::now(),
            },
        );
        Ok(id)
    }
    pub fn uploaded(&self, id: &str) -> Result<u64> {
        self.0
            .uploads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .map(|u| u.size)
            .ok_or_else(|| Error::Invalid("upload not found".into()))
    }
    pub async fn append(&self, id: &str, offset: u64, bytes: &[u8]) -> Result<u64> {
        if bytes.len() > CHUNK {
            return Err(Error::Invalid("append exceeds 4 MiB".into()));
        }
        let _line = self.0.line.lock().await;
        self.check_capacity(bytes.len() as u64)?;
        let mut uploads = self.0.uploads.lock().unwrap_or_else(|e| e.into_inner());
        let upload = uploads
            .get_mut(id)
            .ok_or_else(|| Error::Invalid("upload not found".into()))?;
        if upload.size != offset
            || upload.size.saturating_add(bytes.len() as u64) > self.0.caps.max_file
        {
            return Err(Error::Invalid("invalid upload offset or size".into()));
        }
        let mut file = std::fs::OpenOptions::new().write(true).open(&upload.path)?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(bytes)?;
        file.sync_data()?;
        upload.size += bytes.len() as u64;
        upload.touched = tokio::time::Instant::now();
        Ok(upload.size)
    }
    pub async fn finish(&self, id: &str) -> Result<FileInfo> {
        let _line = self.0.line.lock().await;
        let upload = self
            .0
            .uploads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
            .ok_or_else(|| Error::Invalid("upload not found".into()))?;
        if let Some(info) = upload
            .write
            .key
            .as_deref()
            .and_then(|key| self.by_key(&upload.write.space, key))
        {
            std::fs::remove_file(upload.path)?;
            return Ok(info);
        }
        let before = self.snapshot();
        let mut next = before.clone();
        let blob = new_token(18);
        let target = self.0.dir.join("blobs").join(&blob);
        std::fs::rename(&upload.path, &target)?;
        let info = match self.add(&mut next, upload.write, blob, upload.size) {
            Ok(info) => info,
            Err(error) => {
                let _ = std::fs::remove_file(&target);
                return Err(error);
            }
        };
        if let Err(error) = self.commit(&before, next).await {
            if self.file(&info.id).is_none() {
                let _ = std::fs::remove_file(target);
            }
            return Err(error);
        }
        Ok(info)
    }
    pub fn cancel(&self, id: &str) -> Result<()> {
        if let Some(upload) = self
            .0
            .uploads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
        {
            std::fs::remove_file(upload.path)?;
        }
        Ok(())
    }
    pub fn expire_uploads(&self, idle_seconds: u64) -> Result<usize> {
        let mut uploads = self.0.uploads.lock().unwrap_or_else(|e| e.into_inner());
        let mut count = 0;
        let mut error = None;
        uploads.retain(|_, upload| {
            if upload.touched.elapsed() < Duration::from_secs(idle_seconds) {
                return true;
            }
            match std::fs::remove_file(&upload.path) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => {
                    error = Some(e);
                    return true;
                }
            }
            count += 1;
            false
        });
        error.map_or(Ok(count), |e| Err(e.into()))
    }
    fn listing(&self, space: &str, path: &str) -> Result<JsonValue> {
        let mut entries = self.list(space, path)?;
        let more = entries.len() > FOLDER_VIEW;
        entries.truncate(FOLDER_VIEW);
        Ok(JsonValue::from_serde(
            json!({"entries":entries,"more":more}),
        ))
    }
    pub fn folder(&self, space: &str, path: &str) -> Result<FilesFolder> {
        let source = Source::new(self.0.store.batch(), JsonValue::Null)?;
        self.watch_folder(space, path, source, &Context::background())
    }
    fn watch_folder(
        &self,
        space: &str,
        path: &str,
        source: Source,
        ctx: &Context,
    ) -> Result<FilesFolder> {
        let mut views = self.0.views.lock().unwrap_or_else(|e| e.into_inner());
        source.set(self.listing(space, path)?, ctx)?;
        let id = new_token(12);
        views.insert(id.clone(), (space.into(), path.into(), source.clone()));
        Ok(FilesFolder {
            files: self.clone(),
            id,
            source,
        })
    }
}
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
pub struct FilesFolder {
    files: Files,
    id: String,
    pub source: Source,
}
impl Drop for FilesFolder {
    fn drop(&mut self) {
        self.files
            .0
            .views
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

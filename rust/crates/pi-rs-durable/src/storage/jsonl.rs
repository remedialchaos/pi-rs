use super::memory::{Prepared, State, backend, commit_result, lock, state_reads};
use super::scan::MAX_SAFE_INTEGER;
use super::*;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAIN: &str = "main.jsonl";
const FORMAT: u8 = 1;
const RECLAIM: &str = ".reclaim";
const READ_ONLY_OPEN_ATTEMPTS: usize = 5;

#[derive(Clone, Copy, Debug, Default)]
pub struct JsonlStorageOptions {
    pub fsync: bool,
    pub read_only: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("JSONL corruption: {0}")]
pub struct JsonlCorruptionError(pub String);
#[derive(Debug, thiserror::Error)]
#[error("JSONL storage is open read-only")]
pub struct JsonlStorageReadOnlyError;
#[derive(Debug, thiserror::Error)]
#[error("JSONL storage is poisoned and must be reopened: {0}")]
pub struct JsonlStoragePoisonedError(pub String);

#[derive(Serialize, Deserialize)]
enum MarkerKind {
    #[serde(rename = "commit")]
    Commit,
}
#[derive(Serialize, Deserialize)]
enum RecordKind {
    #[serde(rename = "record")]
    Record,
}
#[derive(Serialize, Deserialize)]
struct Marker {
    format: u8,
    #[serde(rename = "type")]
    kind: MarkerKind,
    seq: Seq,
    writes: Vec<MainOperation>,
}
#[expect(
    clippy::large_enum_variant,
    reason = "Inline terminal task receipts when decoding the main log"
)]
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum MainOperation {
    Conversation {
        value: ConversationRecord,
    },
    Entry {
        value: EntryRecord,
    },
    Submission {
        value: SubmissionRecord,
    },
    Task {
        value: TaskRecord,
    },
    #[serde(rename = "task.sidecar")]
    TaskSidecar {
        id: TaskId,
        ordinal: u64,
    },
    #[serde(rename = "document.create")]
    DocumentCreate {
        record: DocumentCreate,
        ordinal: u64,
    },
    #[serde(rename = "document.change")]
    DocumentChange {
        id: DocumentId,
        ordinal: u64,
    },
    #[serde(rename = "document.retire")]
    DocumentRetire {
        id: DocumentId,
    },
}
impl MainOperation {
    fn document_revision(&self) -> Option<(DocumentId, u64)> {
        match self {
            Self::DocumentCreate { record, ordinal } => Some((record.id, *ordinal)),
            Self::DocumentChange { id, ordinal } => Some((*id, *ordinal)),
            _ => None,
        }
    }
}
#[expect(
    clippy::large_enum_variant,
    reason = "Decode task sidecars directly into their owned record without a second heap allocation"
)]
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Payload {
    Task {
        value: TaskRecord,
    },
    Document {
        id: DocumentId,
        content: DocumentContent,
    },
}
#[derive(Serialize, Deserialize)]
struct Sidecar {
    format: u8,
    #[serde(rename = "type")]
    kind: RecordKind,
    seq: Seq,
    ordinal: u64,
    payload: Payload,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
enum SidecarFile {
    Document(DocumentId),
    Task(TaskId),
}
impl SidecarFile {
    fn parse(name: &str) -> Option<Self> {
        let (kind, id) = name.strip_suffix(".jsonl")?.split_once('-')?;
        if id.is_empty()
            || (id.len() > 1 && id.starts_with('0'))
            || !id.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let id = id.parse().ok()?;
        match kind {
            "doc" => Some(Self::Document(id)),
            "task" => Some(Self::Task(id)),
            _ => None,
        }
    }
    fn name(self) -> String {
        match self {
            Self::Document(id) => format!("doc-{id}.jsonl"),
            Self::Task(id) => format!("task-{id}.jsonl"),
        }
    }
}

struct Active {
    state: State,
    poison: Option<String>,
}
pub struct JsonlStorage {
    path: PathBuf,
    options: JsonlStorageOptions,
    active: Mutex<Option<Active>>,
    commits: Mutex<()>,
}

impl JsonlStorage {
    pub fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        Self::open_with_options(path, JsonlStorageOptions::default())
    }
    pub fn open_with_options(
        path: impl AsRef<Path>,
        options: JsonlStorageOptions,
    ) -> StorageResult<Self> {
        let path = path.as_ref().to_path_buf();
        if !options.read_only {
            fs::create_dir_all(&path)?;
        }
        let mut main_size = 0;
        let mut attempts = 1;
        loop {
            match recover(&path, options, &mut main_size) {
                Ok(state) => {
                    return Ok(Self {
                        path,
                        options,
                        active: Mutex::new(Some(Active {
                            state,
                            poison: None,
                        })),
                        commits: Mutex::new(()),
                    });
                }
                Err(StorageError::JsonlCorruption(_))
                    if options.read_only
                        && attempts < READ_ONLY_OPEN_ATTEMPTS
                        && fs::metadata(path.join(MAIN))
                            .is_ok_and(|metadata| metadata.len() != main_size) =>
                {
                    attempts += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }
    fn read<T>(&self, read: impl FnOnce(&State) -> StorageResult<T>) -> StorageResult<T> {
        let active = lock(&self.active)?;
        let active = active
            .as_ref()
            .ok_or_else(|| backend("JsonlStorage is closed"))?;
        active.usable()?;
        read(&active.state)
    }
    fn append(
        &self,
        markers: &[u8],
        sidecars: &IndexMap<SidecarFile, Vec<u8>>,
    ) -> StorageResult<()> {
        for (file, bytes) in sidecars {
            append(&self.path.join(file.name()), bytes, self.options.fsync)?;
        }
        // Flush directory entries for newly created sidecars before publishing their marker.
        sync_directory(&self.path, self.options.fsync)?;
        append(&self.path.join(MAIN), markers, self.options.fsync)?;
        sync_directory(&self.path, self.options.fsync)?;
        Ok(())
    }
}
impl Active {
    fn usable(&self) -> StorageResult<()> {
        if let Some(error) = &self.poison {
            Err(JsonlStoragePoisonedError(error.clone()).into())
        } else {
            Ok(())
        }
    }
    fn poison(&mut self, error: &StorageError) -> StorageError {
        let cause = error.to_string();
        self.poison = Some(cause.clone());
        JsonlStoragePoisonedError(cause).into()
    }
}
impl Storage for JsonlStorage {
    fn commit(
        &self,
        batches: &[Vec<StorageWrite>],
        receive: &mut dyn FnMut(usize, Seq),
    ) -> StorageResult<CommitResult> {
        let _write = lock(&self.commits)?;
        let mut seqs = Vec::with_capacity(batches.len());
        let mut refused = None;
        let mut markers = Vec::new();
        let mut sidecars = IndexMap::new();
        let mut reclaim = IndexMap::new();
        {
            let mut active = lock(&self.active)?;
            let active = active
                .as_mut()
                .ok_or_else(|| backend("JsonlStorage is closed"))?;
            active.usable()?;
            if self.options.read_only {
                return Err(JsonlStorageReadOnlyError.into());
            }
            for batch in batches {
                let prepared = match active.state.prepare(batch, active.state.next_seq) {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        refused = Some(error);
                        break;
                    }
                };
                if let Err(error) = encode(
                    &prepared,
                    &mut markers,
                    &mut sidecars,
                    &mut reclaim,
                    &active.state,
                ) {
                    return Err(if seqs.is_empty() {
                        error
                    } else {
                        active.poison(&error)
                    });
                }
                seqs.push(prepared.seq);
                active.state.apply(prepared);
            }
            if !seqs.is_empty()
                && let Err(error) = self.append(&markers, &sidecars)
            {
                return Err(active.poison(&error));
            }
        }
        for (index, &seq) in seqs.iter().enumerate() {
            receive(index, seq);
        }
        reclaim_files(&self.path, &sidecars, reclaim, self.options.fsync);
        Ok(commit_result(seqs.len(), refused))
    }
    fn mint_id(&self) -> StorageResult<u64> {
        let mut active = lock(&self.active)?;
        let active = active
            .as_mut()
            .ok_or_else(|| backend("JsonlStorage is closed"))?;
        active.usable()?;
        active.state.mint_id()
    }
    state_reads!();
    fn close(&self) -> StorageResult<()> {
        *lock(&self.active)? = None;
        Ok(())
    }
}

fn line<T: Serialize>(output: &mut Vec<u8>, value: &T) -> StorageResult<()> {
    serde_json::to_writer(&mut *output, value)?;
    output.push(b'\n');
    Ok(())
}

// Reclamation takes effect only after the marker; byte offsets refer to this commit call's aggregated append.
fn encode(
    prepared: &Prepared,
    markers: &mut Vec<u8>,
    sidecars: &mut IndexMap<SidecarFile, Vec<u8>>,
    reclaim: &mut IndexMap<SidecarFile, Option<usize>>,
    state: &State,
) -> StorageResult<()> {
    let seq = prepared.seq;
    let mut ordinal = 0;
    let mut writes = Vec::with_capacity(prepared.writes.len());
    let mut final_tasks = IndexMap::new();
    for write in &prepared.writes {
        let operation = match write {
            StorageWrite::Conversation { value } => MainOperation::Conversation {
                value: value.clone(),
            },
            StorageWrite::Entry { value } => MainOperation::Entry {
                value: value.clone(),
            },
            StorageWrite::Submission { value } => MainOperation::Submission {
                value: value.clone(),
            },
            StorageWrite::Task { value } => {
                let terminal = value.state.status() == TaskStatus::Terminal;
                final_tasks.insert(value.id, terminal);
                if terminal {
                    MainOperation::Task {
                        value: value.clone(),
                    }
                } else {
                    let file = SidecarFile::Task(value.id);
                    let payload = Payload::Task {
                        value: value.clone(),
                    };
                    line(
                        sidecars.entry(file).or_default(),
                        &Sidecar {
                            format: FORMAT,
                            kind: RecordKind::Record,
                            seq,
                            ordinal,
                            payload,
                        },
                    )?;
                    let operation = MainOperation::TaskSidecar {
                        id: value.id,
                        ordinal,
                    };
                    ordinal += 1;
                    operation
                }
            }
            StorageWrite::DocumentCreate { record, content } => {
                let id = record.id;
                let payload = Payload::Document {
                    id,
                    content: content.clone(),
                };
                line(
                    sidecars.entry(SidecarFile::Document(id)).or_default(),
                    &Sidecar {
                        format: FORMAT,
                        kind: RecordKind::Record,
                        seq,
                        ordinal,
                        payload,
                    },
                )?;
                let operation = MainOperation::DocumentCreate {
                    record: record.clone(),
                    ordinal,
                };
                ordinal += 1;
                operation
            }
            StorageWrite::DocumentChange { id, content } => {
                let id = *id;
                let file = SidecarFile::Document(id);
                let bytes = sidecars.entry(file).or_default();
                if matches!(content, DocumentContent::Base { .. })
                    && state
                        .documents
                        .get(&id)
                        .is_some_and(|doc| doc.record.current_only())
                {
                    reclaim.insert(file, Some(bytes.len()));
                }
                let payload = Payload::Document {
                    id,
                    content: content.clone(),
                };
                line(
                    bytes,
                    &Sidecar {
                        format: FORMAT,
                        kind: RecordKind::Record,
                        seq,
                        ordinal,
                        payload,
                    },
                )?;
                let operation = MainOperation::DocumentChange { id, ordinal };
                ordinal += 1;
                operation
            }
            StorageWrite::DocumentRetire { id } => MainOperation::DocumentRetire { id: *id },
            StorageWrite::DocumentCopy { .. } => {
                return Err(backend("Unresolved JSONL document copy"));
            }
        };
        writes.push(operation);
    }
    for (&id, action) in &prepared.actions {
        if action.retire
            && (state
                .documents
                .get(&id)
                .is_some_and(|doc| doc.record.current_only())
                || action.create.as_ref().is_some_and(current_only))
        {
            reclaim.insert(SidecarFile::Document(id), None);
        }
    }
    for (id, terminal) in final_tasks {
        let file = SidecarFile::Task(id);
        if terminal {
            reclaim.insert(file, None);
        } else if reclaim.get(&file) == Some(&None) {
            // The latest task is live again. Older live transitions are required by recovery.
            reclaim.shift_remove(&file);
        }
    }
    line(
        markers,
        &Marker {
            format: FORMAT,
            kind: MarkerKind::Commit,
            seq,
            writes,
        },
    )
}
fn current_only(record: &DocumentCreate) -> bool {
    !matches!(record.scope, DocumentScope::Conversation { .. })
        || record.history != Some(DocumentHistory::Rewindable)
}
fn append(path: &Path, bytes: &[u8], fsync: bool) -> StorageResult<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(bytes)?;
    if fsync {
        file.sync_all()?;
    }
    Ok(())
}
#[cfg(unix)]
fn sync_directory(path: &Path, fsync: bool) -> std::io::Result<()> {
    if fsync {
        File::open(path)?.sync_all()?;
    }
    Ok(())
}
#[cfg(not(unix))]
fn sync_directory(_path: &Path, _fsync: bool) -> std::io::Result<()> {
    Ok(())
}
fn replace(path: &Path, file: SidecarFile, bytes: &[u8], fsync: bool) -> std::io::Result<()> {
    let name = file.name();
    let final_path = path.join(&name);
    if bytes.is_empty() {
        match fs::remove_file(final_path) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    } else {
        let temp = path.join(format!("{name}{RECLAIM}"));
        let mut output = File::create(&temp)?;
        output.write_all(bytes)?;
        if fsync {
            output.sync_all()?;
        }
        drop(output);
        fs::rename(temp, final_path)?;
    }
    sync_directory(path, fsync)
}
fn reclaim_files(
    path: &Path,
    sidecars: &IndexMap<SidecarFile, Vec<u8>>,
    reclaim: IndexMap<SidecarFile, Option<usize>>,
    fsync: bool,
) {
    for (file, offset) in reclaim {
        let bytes = offset
            .and_then(|offset| sidecars.get(&file).and_then(|bytes| bytes.get(offset..)))
            .unwrap_or_default();
        // The commit succeeded even if best-effort maintenance cannot replace its sidecar.
        let _ = replace(path, file, bytes, fsync);
    }
}

struct Parsed<T> {
    lines: Vec<(usize, T)>,
    complete: usize,
    size: usize,
}
fn parse_lines<T: DeserializeOwned>(path: &Path) -> StorageResult<Parsed<T>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, text) in bytes[..complete]
        .split_inclusive(|byte| *byte == b'\n')
        .enumerate()
    {
        // TextDecoder strips a leading UTF-8 BOM for each decoded line.
        let raw = text.strip_prefix(b"\xef\xbb\xbf").unwrap_or(text);
        let value = serde_json::from_slice(raw).map_err(|error| {
            corruption(format!(
                "Malformed complete {} line {}: {error}",
                path.display(),
                index + 1
            ))
        })?;
        lines.push((start, value));
        start += text.len();
    }
    Ok(Parsed {
        lines,
        complete,
        size: bytes.len(),
    })
}
fn corruption(message: impl Into<String>) -> StorageError {
    JsonlCorruptionError(message.into()).into()
}
fn truncate(path: &Path, size: usize) -> StorageResult<()> {
    OpenOptions::new()
        .write(true)
        .open(path)?
        .set_len(size as u64)?;
    Ok(())
}

type SidecarKey = (SidecarFile, Seq, u64);
fn confirm<'a>(
    records: &HashMap<SidecarKey, &'a Sidecar>,
    confirmed: &mut HashSet<SidecarKey>,
    key: SidecarKey,
    optional: bool,
) -> StorageResult<Option<&'a Sidecar>> {
    let value = records.get(&key).copied();
    if let Some(value) = value {
        if !confirmed.insert(key) {
            return Err(corruption("Sidecar record is confirmed more than once"));
        }
        Ok(Some(value))
    } else if optional {
        Ok(None)
    } else {
        Err(corruption(format!(
            "Missing confirmed sidecar record {} at sequence {}",
            key.0.name(),
            key.1
        )))
    }
}

fn recover(path: &Path, options: JsonlStorageOptions, main_size: &mut u64) -> StorageResult<State> {
    let main_path = path.join(MAIN);
    *main_size = fs::metadata(&main_path).map_or(0, |metadata| metadata.len());
    let main = parse_lines::<Marker>(&main_path)?;
    *main_size = main.size as u64;
    let mut prior = 0;
    let mut current_only_docs = HashSet::new();
    let mut retired = HashSet::new();
    let mut terminal_tasks = HashSet::new();
    for (_, marker) in &main.lines {
        if marker.format != FORMAT || marker.seq <= prior || marker.seq > MAX_SAFE_INTEGER {
            return Err(corruption(
                "Invalid or non-increasing main.jsonl commit marker",
            ));
        }
        prior = marker.seq;
        for op in &marker.writes {
            match op {
                MainOperation::DocumentCreate { record, .. } if current_only(record) => {
                    current_only_docs.insert(record.id);
                }
                MainOperation::DocumentRetire { id } => {
                    retired.insert(*id);
                }
                MainOperation::Task { value } => {
                    if value.state.status() != TaskStatus::Terminal {
                        return Err(corruption("Invalid terminal task write in main.jsonl"));
                    }
                    terminal_tasks.insert(value.id);
                }
                MainOperation::TaskSidecar { id, .. } => {
                    terminal_tasks.remove(id);
                }
                _ => {}
            }
        }
    }
    retired.retain(|id| current_only_docs.contains(id));
    let mut files = Vec::new();
    let mut empty_reclaims = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(file) = SidecarFile::parse(name) {
            files.push((file, parse_lines::<Sidecar>(&entry.path())?));
        } else if name
            .strip_suffix(RECLAIM)
            .and_then(SidecarFile::parse)
            .is_some()
        {
            empty_reclaims.push(entry.path());
        }
    }
    files.sort_unstable_by_key(|(file, _)| *file);
    let mut records = HashMap::new();
    for (file, parsed) in &files {
        let mut prior = (0, 0);
        for (_, record) in &parsed.lines {
            let position = (record.seq, record.ordinal);
            if record.format != FORMAT
                || record.seq == 0
                || record.seq > MAX_SAFE_INTEGER
                || record.ordinal > MAX_SAFE_INTEGER
                || position <= prior
            {
                return Err(corruption(format!(
                    "Invalid or out-of-order sidecar record in {}",
                    file.name()
                )));
            }
            match &record.payload {
                Payload::Task { value }
                    if value.id <= MAX_SAFE_INTEGER
                        && value.state.status() != TaskStatus::Terminal => {}
                Payload::Document { id, content }
                    if *id <= MAX_SAFE_INTEGER
                        && (1..=MAX_SAFE_INTEGER).contains(&content.version()) => {}
                _ => return Err(corruption(format!("Invalid payload in {}", file.name()))),
            }
            prior = position;
            records.insert((*file, record.seq, record.ordinal), record);
        }
    }
    let mut latest_bases = HashMap::new();
    for (_, marker) in &main.lines {
        for op in &marker.writes {
            if let Some((id, ordinal)) = op.document_revision()
                && current_only_docs.contains(&id)
            {
                let key = (SidecarFile::Document(id), marker.seq, ordinal);
                if records.get(&key).is_some_and(|record|matches!(&record.payload,Payload::Document {id: found,content:DocumentContent::Base {..}} if *found==id)) { latest_bases.insert(id,(marker.seq,ordinal)); }
            }
        }
    }
    let mut confirmed = HashSet::new();
    let mut state = State::default();
    for (_, marker) in main.lines {
        let mut writes = Vec::with_capacity(marker.writes.len());
        for op in marker.writes {
            match op {
                MainOperation::Conversation { value } => {
                    writes.push(StorageWrite::Conversation { value })
                }
                MainOperation::Entry { value } => writes.push(StorageWrite::Entry { value }),
                MainOperation::Submission { value } => {
                    writes.push(StorageWrite::Submission { value })
                }
                MainOperation::Task { value } => writes.push(StorageWrite::Task { value }),
                MainOperation::DocumentRetire { id } => {
                    writes.push(StorageWrite::DocumentRetire { id })
                }
                MainOperation::TaskSidecar { id, ordinal } => {
                    let optional = terminal_tasks.contains(&id);
                    if id > MAX_SAFE_INTEGER || ordinal > MAX_SAFE_INTEGER {
                        return Err(corruption("Invalid task sidecar reference"));
                    }
                    if let Some(record) = confirm(
                        &records,
                        &mut confirmed,
                        (SidecarFile::Task(id), marker.seq, ordinal),
                        optional,
                    )? {
                        match &record.payload {
                            Payload::Task { value } if value.id == id => {
                                if !optional {
                                    writes.push(StorageWrite::Task {
                                        value: value.clone(),
                                    });
                                }
                            }
                            _ => {
                                return Err(corruption(format!(
                                    "Confirmed task sidecar data does not match commit {}",
                                    marker.seq
                                )));
                            }
                        }
                    }
                }
                op @ (MainOperation::DocumentCreate { .. }
                | MainOperation::DocumentChange { .. }) => {
                    let (id, ordinal) = op
                        .document_revision()
                        .ok_or_else(|| corruption("Invalid document sidecar operation"))?;
                    if id > MAX_SAFE_INTEGER || ordinal > MAX_SAFE_INTEGER {
                        return Err(corruption("Invalid document sidecar reference"));
                    }
                    let optional = retired.contains(&id)
                        || latest_bases
                            .get(&id)
                            .is_some_and(|&base| (marker.seq, ordinal) < base);
                    let content = confirm(
                        &records,
                        &mut confirmed,
                        (SidecarFile::Document(id), marker.seq, ordinal),
                        optional,
                    )?
                    .map(|record| match &record.payload {
                        Payload::Document { id: found, content } if *found == id => Ok(content),
                        _ => Err(corruption(format!(
                            "Confirmed document sidecar data does not match commit {}",
                            marker.seq
                        ))),
                    })
                    .transpose()?;
                    if let MainOperation::DocumentCreate { record, .. } = op {
                        if content
                            .is_some_and(|content| !matches!(content, DocumentContent::Base { .. }))
                        {
                            return Err(corruption("Document creation lacks a confirmed base"));
                        }
                        // Preserve lifecycle/address metadata even when a later marker has made this base unreachable.
                        let content = if optional {
                            DocumentContent::Base {
                                version: 1,
                                value: JsonObject::new(),
                            }
                        } else {
                            content.cloned().ok_or_else(|| {
                                corruption("Document creation lacks a confirmed base")
                            })?
                        };
                        writes.push(StorageWrite::DocumentCreate { record, content });
                    } else if !optional && let Some(content) = content {
                        writes.push(StorageWrite::DocumentChange {
                            id,
                            content: content.clone(),
                        });
                    }
                }
            }
        }
        let prepared = state.prepare(&writes, marker.seq).map_err(|error| {
            corruption(format!(
                "Invalid committed state at sequence {}: {error}",
                marker.seq
            ))
        })?;
        state.apply(prepared);
    }
    for (file, parsed) in files {
        let mut end = parsed.complete;
        let mut saw_unconfirmed = false;
        let mut obsolete = 0;
        let mut retained = Vec::new();
        for (offset, record) in parsed.lines {
            let position = (record.seq, record.ordinal);
            if confirmed.contains(&(file, position.0, position.1)) {
                if saw_unconfirmed {
                    return Err(corruption(format!(
                        "Confirmed record follows an unconfirmed tail in {}",
                        file.name()
                    )));
                }
                let keep = match file {
                    SidecarFile::Task(id) => !terminal_tasks.contains(&id),
                    SidecarFile::Document(id) => {
                        !retired.contains(&id)
                            && latest_bases.get(&id).is_none_or(|&base| position >= base)
                    }
                };
                if !keep {
                    obsolete += 1;
                } else if !options.read_only && obsolete > 0 {
                    line(&mut retained, &record)?;
                }
            } else if !saw_unconfirmed {
                saw_unconfirmed = true;
                end = offset;
            }
        }
        if !options.read_only {
            if end != parsed.size {
                truncate(&path.join(file.name()), end)?;
            }
            if obsolete > 0 {
                let _ = replace(path, file, &retained, options.fsync);
            }
        }
    }
    if !options.read_only {
        if main.complete != main.size {
            truncate(&main_path, main.complete)?;
        }
        for file in empty_reclaims {
            let _ = fs::remove_file(file);
        }
    }
    Ok(state)
}

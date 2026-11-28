//! Gzip JSONL v2 and the directory-backed sync remote. Bodies are Read/Write streams.
mod client;
mod scheduler;
use crate::{Error, Result, store::OutboxCommit};
pub use client::{
    AsyncRemote, Body, MemorySyncState, SyncClient, SyncFuture, SyncState, SyncStateStore,
    UploadResult,
};
use flate2::{Compression, read::MultiGzDecoder, write::GzEncoder};
pub use scheduler::{FlushOptions, FlushScheduler};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};
pub const LOG_VERSION: u32 = 2;
pub const LOG_START: u64 = 0;
pub const SYNC_PROTOCOL: u32 = 1;
pub const UPLOAD_BYTES: usize = 8 * 1024 * 1024;
pub const PULL_BYTES: usize = 4 * 1024 * 1024;
pub const OUTBOX_PAGE: usize = 100;
pub const OUTBOX_PAGE_BYTES: usize = 1024 * 1024;
fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}
fn safe_int(v: &Value, min: i64) -> Option<i64> {
    v.as_f64()
        .filter(|n| n.fract() == 0.0 && *n >= min as f64 && n.abs() <= 9_007_199_254_740_991.0)
        .map(|n| n as i64)
}
pub fn encode_log_line(commit: &OutboxCommit) -> Result<String> {
    Ok(format!(
        "{}\n",
        crate::json::stringify(
            &json!({"v":LOG_VERSION,"seq":commit.seq,"nextId":commit.next_id,"writes":commit.writes})
        )?
    ))
}
pub fn decode_log_line(text: &str, number: usize) -> Result<OutboxCommit> {
    let line: Value = serde_json::from_str(text)
        .map_err(|_| invalid(format!("Log line {number} is not JSON")))?;
    if !line.is_object() {
        return Err(invalid(format!("Log line {number} is not an object")));
    }
    if line.get("v").and_then(|v| safe_int(v, 0)) != Some(LOG_VERSION as i64) {
        return Err(invalid(format!(
            "Log line {number} has unknown version {}",
            line.get("v").map_or("undefined".into(), Value::to_string)
        )));
    }
    let seq = line
        .get("seq")
        .and_then(|v| safe_int(v, 1))
        .ok_or_else(|| invalid(format!("Log line {number} has no valid sequence")))?;
    let next_id = line
        .get("nextId")
        .and_then(|v| safe_int(v, -9_007_199_254_740_991))
        .ok_or_else(|| invalid(format!("Log line {number} has no valid next id")))?;
    let writes = line
        .get("writes")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(format!("Log line {number} has no writes")))?
        .clone();
    Ok(OutboxCommit {
        seq,
        next_id,
        writes,
    })
}
pub fn encode_batch<W: Write>(
    writer: W,
    commits: impl IntoIterator<Item = OutboxCommit>,
) -> Result<W> {
    let mut gzip = GzEncoder::new(writer, Compression::default());
    for commit in commits {
        gzip.write_all(encode_log_line(&commit)?.as_bytes())?;
    }
    Ok(gzip.finish()?)
}
/// Incremental decompression and strict UTF-8. Dropping the iterator closes an owned input file.
pub struct BatchReader<R: Read> {
    reader: BufReader<MultiGzDecoder<R>>,
    previous: i64,
    number: usize,
    done: bool,
}
impl<R: Read> BatchReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(MultiGzDecoder::new(reader)),
            previous: 0,
            number: 0,
            done: false,
        }
    }
}
impl<R: Read> Iterator for BatchReader<R> {
    type Item = Result<OutboxCommit>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let result = (|| {
            let mut bytes = Vec::new();
            let count = self.reader.read_until(b'\n', &mut bytes)?;
            if count == 0 {
                return Ok(None);
            }
            self.number += 1;
            if bytes.last() != Some(&b'\n') {
                return Err(invalid(format!(
                    "The body ends inside log line {}",
                    self.number
                )));
            }
            bytes.pop();
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| invalid(format!("Log line {} is not UTF-8", self.number)))?;
            let commit = decode_log_line(text, self.number)?;
            if commit.seq <= self.previous {
                return Err(invalid(format!(
                    "Log line {} does not increase the sequence",
                    self.number
                )));
            }
            self.previous = commit.seq;
            Ok(Some(commit))
        })();
        match result {
            Ok(Some(commit)) => Some(Ok(commit)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(error) => {
                self.done = true;
                Some(Err(error))
            }
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchInfo {
    pub after: u64,
    pub to: u64,
    pub device: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folded: Option<bool>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteListing {
    pub batches: Vec<BatchInfo>,
    pub snapshots: Vec<u64>,
}
#[derive(Debug, PartialEq, Eq)]
pub enum PutBatchResult {
    Created,
    Exists { to: u64, device: String },
    NotHead { head: u64 },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteHistory {
    pub snapshot: Option<u64>,
    pub batches: Vec<BatchInfo>,
    pub head: u64,
}
/// Resume inside a batch, but only restore from an archive when starting an empty store.
pub fn remote_history(listing: &RemoteListing, from: Option<u64>) -> Result<RemoteHistory> {
    let restore = from.unwrap_or(0) == 0;
    let mut snapshot = restore
        .then(|| listing.snapshots.iter().copied().max())
        .flatten();
    let mut at = snapshot.or(from).unwrap_or(0);
    let mut batches = Vec::new();
    if !restore && let Some(batch) = listing.batches.iter().find(|b| b.after < at && at < b.to) {
        at = batch.to;
        batches.push(batch.clone());
    }
    let chain: HashMap<_, _> = listing.batches.iter().map(|b| (b.after, b)).collect();
    let mut seen = HashSet::new();
    while let Some(batch) = chain.get(&at) {
        if !seen.insert(at) || batch.to <= at {
            return Err(invalid("sync: cyclic or non-increasing history"));
        }
        at = batch.to;
        batches.push((*batch).clone());
    }
    if restore && let Some(index) = batches.iter().rposition(|b| b.archive == Some(true)) {
        batches.drain(..index);
        snapshot = None;
    }
    Ok(RemoteHistory {
        snapshot,
        batches,
        head: at,
    })
}
pub fn remote_head(listing: &RemoteListing) -> Result<u64> {
    let chain: HashMap<_, _> = listing
        .batches
        .iter()
        .map(|batch| (batch.after, batch))
        .collect();
    let mut at = 0;
    let mut seen = HashSet::new();
    while let Some(next) = chain.get(&at) {
        if !seen.insert(at) || next.to <= at {
            return Err(invalid("sync: cyclic or non-increasing history"));
        }
        at = next.to;
    }
    Ok(at)
}
pub fn check_session_id(id: &str) -> Result<&str> {
    let mut bytes = id.bytes();
    let first = bytes
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-');
    if !first || !bytes.all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)) {
        return Err(invalid(format!(
            "Invalid session ID for a sync directory: {id}"
        )));
    }
    Ok(id)
}
#[derive(Clone)]
pub struct FileSystemSyncRemote {
    root: PathBuf,
}
impl FileSystemSyncRemote {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    fn path(&self, id: &str, directory: &str, name: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join(check_session_id(id)?)
            .join(directory)
            .join(name))
    }
    fn read(&self, id: &str, directory: &str, name: &str) -> Result<Option<Vec<u8>>> {
        let path = self.path(id, directory, name)?;
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn write(&self, id: &str, directory: &str, name: &str, mut body: impl Read) -> Result<()> {
        let path = self.path(id, directory, name)?;
        let directory = path
            .parent()
            .ok_or_else(|| invalid("sync: file has no parent"))?;
        fs::create_dir_all(directory)?;
        let temporary = directory.join(format!("{name}.tmp"));
        let result = (|| {
            let mut writer = File::create(&temporary)?;
            std::io::copy(&mut body, &mut writer)?;
            drop(writer);
            fs::rename(&temporary, &path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
    fn slot(&self, id: &str, after: u64) -> Result<Option<BatchInfo>> {
        self.read(id, "slots", &format!("{after:016}.json"))?
            .map(|v| serde_json::from_slice(&v).map_err(Error::from))
            .transpose()
    }
    pub fn put_batch(
        &self,
        id: &str,
        after: u64,
        to: u64,
        device: &str,
        body: impl Read,
        archive: bool,
    ) -> Result<PutBatchResult> {
        if let Some(existing) = self.slot(id, after)? {
            return Ok(PutBatchResult::Exists {
                to: existing.to,
                device: existing.device,
            });
        }
        let head = remote_head(&self.list(id)?)?;
        if after != head {
            return Ok(PutBatchResult::NotHead { head });
        }
        self.write(id, "batches", &format!("{after:016}.batch"), body)?;
        let slot = BatchInfo {
            after,
            to,
            device: device.into(),
            archive: archive.then_some(true),
            folded: None,
        };
        let bytes = serde_json::to_vec(&slot)?;
        self.write(id, "slots", &format!("{after:016}.json"), bytes.as_slice())?;
        Ok(PutBatchResult::Created)
    }
    fn open(&self, id: &str, directory: &str, name: &str) -> Result<Option<File>> {
        match File::open(self.path(id, directory, name)?) {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn get_batch(&self, id: &str, after: u64) -> Result<Option<File>> {
        if self.slot(id, after)?.is_none_or(|s| s.folded == Some(true)) {
            return Ok(None);
        }
        self.open(id, "batches", &format!("{after:016}.batch"))
    }
    pub fn put_snapshot(&self, id: &str, to: u64, body: impl Read) -> Result<()> {
        self.write(id, "snapshots", &format!("{to:016}.batch"), body)
    }
    pub fn get_snapshot(&self, id: &str, to: u64) -> Result<Option<File>> {
        self.open(id, "snapshots", &format!("{to:016}.batch"))
    }
    pub fn put_meta(&self, id: &str, meta: &Value) -> Result<()> {
        self.write(
            id,
            "",
            "meta.json",
            crate::json::stringify(meta)?.as_bytes(),
        )
    }
    pub fn get_meta(&self, id: &str) -> Result<Option<Value>> {
        self.read(id, "", "meta.json")?
            .map(|v| serde_json::from_slice(&v).map_err(Error::from))
            .transpose()
    }
    fn names(path: &Path) -> Result<Vec<String>> {
        match fs::read_dir(path) {
            Ok(entries) => entries
                .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn list(&self, id: &str) -> Result<RemoteListing> {
        let mut listing = RemoteListing::default();
        for name in Self::names(&self.path(id, "slots", "")?)? {
            if let Some(seq) = file_sequence(&name, ".json")
                && let Some(slot) = self.slot(id, seq)?
            {
                listing.batches.push(slot);
            }
        }
        for name in Self::names(&self.path(id, "snapshots", "")?)? {
            if let Some(seq) = file_sequence(&name, ".batch") {
                listing.snapshots.push(seq);
            }
        }
        Ok(listing)
    }
    pub fn fold_batches(&self, id: &str, afters: &[u64]) -> Result<()> {
        for after in afters {
            if let Some(mut slot) = self.slot(id, *after)? {
                if slot.folded == Some(true) {
                    continue;
                }
                slot.folded = Some(true);
                let bytes = serde_json::to_vec(&slot)?;
                self.write(id, "slots", &format!("{after:016}.json"), bytes.as_slice())?;
                remove_optional(&self.path(id, "batches", &format!("{after:016}.batch"))?)?;
            }
        }
        Ok(())
    }
    pub fn delete_snapshots(&self, id: &str, sequences: &[u64]) -> Result<()> {
        for seq in sequences {
            remove_optional(&self.path(id, "snapshots", &format!("{seq:016}.batch"))?)?;
        }
        Ok(())
    }
    pub fn sessions(&self) -> Result<Vec<(String, u64, Option<Value>)>> {
        let mut names = Self::names(&self.root)?;
        names.sort();
        let mut result = Vec::new();
        for name in names {
            if check_session_id(&name).is_ok() && self.root.join(&name).is_dir() {
                let head = remote_head(&self.list(&name)?)?;
                let meta = self.get_meta(&name)?;
                result.push((name, head, meta));
            }
        }
        Ok(result)
    }
}
fn file_sequence(name: &str, suffix: &str) -> Option<u64> {
    let n = name.strip_suffix(suffix)?;
    if n.len() == 16 && n.bytes().all(|c| c.is_ascii_digit()) {
        n.parse().ok()
    } else {
        None
    }
}
fn remove_optional(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

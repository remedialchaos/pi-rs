//! Rust port of `packages/coding-agent/src/core/session-manager.ts`.
//!
//! A session is an append-only tree of JSONL entries. Every entry carries an
//! `id` and a `parentId`; a leaf pointer selects the active path. Context is
//! rebuilt by walking from the root to the leaf, applying compaction and
//! context edits along the way.
//!
//! The module intentionally uses `serde_json::Value` for message payloads so it
//! stays independent of the AI crate's concrete message types. Serialized
//! shapes match upstream JSONL v3 exactly (internally tagged `type`, camelCase
//! field names, omitted optional fields).

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Version written into new session headers and the version migrations target.
pub const CURRENT_SESSION_VERSION: u32 = 3;

const SESSION_HEADER_SCAN_LIMIT: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// First line of a session file. Upstream calls this a `SessionHeader`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "session")]
pub struct SessionHeader {
    /// Older (v1) files have no version field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    pub id: String,
    pub timestamp: String,
    /// Upstream may write sessions without a cwd; treat that as an empty string.
    #[serde(default)]
    pub cwd: String,
    #[serde(
        rename = "parentSession",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_session: Option<String>,
}

/// One JSONL record after the header. All upstream v3 variants are represented.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum SessionEntry {
    #[serde(rename = "message")]
    Message {
        id: String,
        parent_id: Option<String>,
        timestamp: String,
        message: Value,
    },
    #[serde(rename = "thinking_level_change")]
    ThinkingLevelChange {
        id: String,
        parent_id: Option<String>,
        timestamp: String,
        thinking_level: String,
    },
    #[serde(rename = "model_change")]
    ModelChange {
        id: String,
        parent_id: Option<String>,
        timestamp: String,
        provider: String,
        model_id: String,
    },
    #[serde(rename = "usage")]
    Usage {
        id: String,
        parent_id: Option<String>,
        timestamp: String,
        kind: String,
        provider: String,
        model: String,
        usage: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    #[serde(rename = "compaction")]
    Compaction {
        id: String,
        parent_id: Option<String>,
        timestamp: String,
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_message: Option<Value>,
    },
    #[serde(rename = "branch_summary")]
    BranchSummary {
        id: String,
        parent_id: Option<String>,
        timestamp: String,
        from_id: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
    },
    #[serde(rename = "custom")]
    Custom {
        id: String,
        parent_id: Option<String>,
        timestamp: String,
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    #[serde(rename = "custom_message")]
    CustomMessage {
        id: String,
        parent_id: Option<String>,
        timestamp: String,
        custom_type: String,
        content: Value,
        display: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    #[serde(rename = "context_edit")]
    ContextEdit {
        id: String,
        parent_id: Option<String>,
        timestamp: String,
        target_id: String,
        replacement: Value,
    },
    #[serde(rename = "label")]
    Label {
        id: String,
        parent_id: Option<String>,
        timestamp: String,
        target_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    #[serde(rename = "session_info")]
    SessionInfoEntry {
        id: String,
        parent_id: Option<String>,
        timestamp: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

impl SessionEntry {
    /// The upstream `type` discriminator.
    pub fn entry_type(&self) -> &'static str {
        match self {
            Self::Message { .. } => "message",
            Self::ThinkingLevelChange { .. } => "thinking_level_change",
            Self::ModelChange { .. } => "model_change",
            Self::Usage { .. } => "usage",
            Self::Compaction { .. } => "compaction",
            Self::BranchSummary { .. } => "branch_summary",
            Self::Custom { .. } => "custom",
            Self::CustomMessage { .. } => "custom_message",
            Self::ContextEdit { .. } => "context_edit",
            Self::Label { .. } => "label",
            Self::SessionInfoEntry { .. } => "session_info",
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Message { id, .. }
            | Self::ThinkingLevelChange { id, .. }
            | Self::ModelChange { id, .. }
            | Self::Usage { id, .. }
            | Self::Compaction { id, .. }
            | Self::BranchSummary { id, .. }
            | Self::Custom { id, .. }
            | Self::CustomMessage { id, .. }
            | Self::ContextEdit { id, .. }
            | Self::Label { id, .. }
            | Self::SessionInfoEntry { id, .. } => id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Self::Message { parent_id, .. }
            | Self::ThinkingLevelChange { parent_id, .. }
            | Self::ModelChange { parent_id, .. }
            | Self::Usage { parent_id, .. }
            | Self::Compaction { parent_id, .. }
            | Self::BranchSummary { parent_id, .. }
            | Self::Custom { parent_id, .. }
            | Self::CustomMessage { parent_id, .. }
            | Self::ContextEdit { parent_id, .. }
            | Self::Label { parent_id, .. }
            | Self::SessionInfoEntry { parent_id, .. } => parent_id.as_deref(),
        }
    }

    pub fn timestamp(&self) -> &str {
        match self {
            Self::Message { timestamp, .. }
            | Self::ThinkingLevelChange { timestamp, .. }
            | Self::ModelChange { timestamp, .. }
            | Self::Usage { timestamp, .. }
            | Self::Compaction { timestamp, .. }
            | Self::BranchSummary { timestamp, .. }
            | Self::Custom { timestamp, .. }
            | Self::CustomMessage { timestamp, .. }
            | Self::ContextEdit { timestamp, .. }
            | Self::Label { timestamp, .. }
            | Self::SessionInfoEntry { timestamp, .. } => timestamp,
        }
    }

    pub fn set_id(&mut self, value: String) {
        match self {
            Self::Message { id, .. }
            | Self::ThinkingLevelChange { id, .. }
            | Self::ModelChange { id, .. }
            | Self::Usage { id, .. }
            | Self::Compaction { id, .. }
            | Self::BranchSummary { id, .. }
            | Self::Custom { id, .. }
            | Self::CustomMessage { id, .. }
            | Self::ContextEdit { id, .. }
            | Self::Label { id, .. }
            | Self::SessionInfoEntry { id, .. } => *id = value,
        }
    }

    pub fn set_parent_id(&mut self, value: Option<String>) {
        match self {
            Self::Message { parent_id, .. }
            | Self::ThinkingLevelChange { parent_id, .. }
            | Self::ModelChange { parent_id, .. }
            | Self::Usage { parent_id, .. }
            | Self::Compaction { parent_id, .. }
            | Self::BranchSummary { parent_id, .. }
            | Self::Custom { parent_id, .. }
            | Self::CustomMessage { parent_id, .. }
            | Self::ContextEdit { parent_id, .. }
            | Self::Label { parent_id, .. }
            | Self::SessionInfoEntry { parent_id, .. } => *parent_id = value,
        }
    }

    pub fn set_timestamp(&mut self, value: String) {
        match self {
            Self::Message { timestamp, .. }
            | Self::ThinkingLevelChange { timestamp, .. }
            | Self::ModelChange { timestamp, .. }
            | Self::Usage { timestamp, .. }
            | Self::Compaction { timestamp, .. }
            | Self::BranchSummary { timestamp, .. }
            | Self::Custom { timestamp, .. }
            | Self::CustomMessage { timestamp, .. }
            | Self::ContextEdit { timestamp, .. }
            | Self::Label { timestamp, .. }
            | Self::SessionInfoEntry { timestamp, .. } => *timestamp = value,
        }
    }
}

/// Defensive copy of one node returned by [`SessionManager::get_tree`].
#[derive(Debug, Clone, PartialEq)]
pub struct SessionTreeNode {
    pub entry: SessionEntry,
    pub children: Vec<SessionTreeNode>,
    pub label: Option<String>,
    pub label_timestamp: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionContext {
    pub messages: Vec<Value>,
    pub thinking_level: String,
    pub model: Option<ModelRef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedSessionEntry {
    pub source_entry: SessionEntry,
    pub messages: Vec<Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionProjection {
    pub entries: Vec<ProjectedSessionEntry>,
    pub messages: Vec<Value>,
    pub thinking_level: String,
    pub model: Option<ModelRef>,
}

/// Metadata returned by session discovery/listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub id: String,
    pub cwd: String,
    pub name: Option<String>,
    pub parent_session_path: Option<PathBuf>,
    /// ISO-8601 UTC timestamp from the session header.
    pub created: String,
    /// ISO-8601 UTC timestamp: last message activity, header time or file mtime.
    pub modified: String,
    pub created_millis: i64,
    pub modified_millis: i64,
    pub message_count: usize,
    pub first_message: String,
    pub all_messages_text: String,
}

/// Optional fields of a compaction entry.
#[derive(Debug, Clone, Default)]
pub struct CompactionExtra {
    pub details: Option<Value>,
    pub usage: Option<Value>,
    pub from_hook: Option<bool>,
    pub system_message: Option<Value>,
}

/// Optional fields of a branch summary entry.
#[derive(Debug, Clone, Default)]
pub struct BranchSummaryExtra {
    pub details: Option<Value>,
    pub usage: Option<Value>,
    pub from_hook: Option<bool>,
}

// ---------------------------------------------------------------------------
// Small helpers (no external time/uuid crates)
// ---------------------------------------------------------------------------

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn random_u64() -> u64 {
    let mut bytes = [0u8; 8];
    if File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok()
    {
        return u64::from_ne_bytes(bytes);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let mut x = now ^ (counter.wrapping_mul(0x9e37_79b9_7f4a_7c15)) ^ (std::process::id() as u64);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

/// UUIDv7-compatible session id (matches upstream's `uuidv7()` shape).
pub fn create_session_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let a = random_u64();
    let b = random_u64();
    let mut bytes = [0u8; 16];
    bytes[0..6].copy_from_slice(&millis.to_be_bytes()[2..8]);
    bytes[6] = 0x70 | ((a & 0x0f) as u8);
    bytes[7] = ((a >> 8) & 0xff) as u8;
    bytes[8] = 0x80 | ((a >> 16) & 0x3f) as u8;
    bytes[9..16].copy_from_slice(&b.to_be_bytes()[1..8]);
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

/// Upstream `generateId()`: eight hex characters, collision-checked.
fn generate_short_id(used: &HashSet<String>) -> String {
    for _ in 0..100 {
        let id = format!("{:08x}", (random_u64() & 0xffff_ffff) as u32);
        if !used.contains(&id) {
            return id;
        }
    }
    create_session_id()
}

/// Upstream `assertValidSessionId()`.
pub fn assert_valid_session_id(id: &str) -> Result<(), String> {
    let bytes = id.as_bytes();
    let valid = !bytes.is_empty()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_' || *b == b'.')
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric();
    if valid {
        Ok(())
    } else {
        Err("Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character".to_owned())
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    y += i64::from(m <= 2);
    (y, m, d)
}

fn timestamp_now() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    format_timestamp(millis)
}

fn format_timestamp(millis: i64) -> String {
    let days = millis.div_euclid(86_400_000);
    let rem = millis.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let hours = rem / 3_600_000;
    let minutes = (rem % 3_600_000) / 60_000;
    let seconds = (rem % 60_000) / 1000;
    let ms = rem % 1000;
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{ms:03}Z")
}

/// Parse the subset of RFC 3339 emitted by JavaScript's `Date.toISOString()`.
/// Invalid input yields 0, mirroring a best-effort `getTime()` for stored data.
fn parse_timestamp_millis(value: &str) -> i64 {
    let bytes = value.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return 0;
    }
    let number = |start: usize, len: usize| -> Option<i64> {
        let slice = bytes.get(start..start + len)?;
        let text = std::str::from_utf8(slice).ok()?;
        text.parse::<i64>().ok()
    };
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        number(0, 4),
        number(5, 2),
        number(8, 2),
        number(11, 2),
        number(14, 2),
        number(17, 2),
    ) else {
        return 0;
    };
    let mut millis =
        (days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second) * 1000;
    let mut index = 19;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let mut factor = 100;
        while let Some(byte) = bytes.get(index) {
            if !byte.is_ascii_digit() {
                break;
            }
            millis += i64::from(byte - b'0') * factor;
            factor /= 10;
            index += 1;
        }
    }
    match bytes.get(index) {
        Some(b'Z') => millis,
        Some(b'+') | Some(b'-') => {
            let sign = if bytes[index] == b'+' { 1 } else { -1 };
            let offset_hours = number(index + 1, 2).unwrap_or(0);
            let offset_minutes = number(index + 4, 2).unwrap_or(0);
            millis - sign * (offset_hours * 3600 + offset_minutes * 60) * 1000
        }
        _ => millis,
    }
}

fn field_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn message_role(message: &Value) -> Option<&str> {
    field_str(message, "role")
}

fn is_system_entry(value: &Value) -> bool {
    field_str(value, "type") == Some("session")
}

fn io_err(err: std::io::Error) -> String {
    err.to_string()
}

/// Lexically resolve a path against the current working directory.
fn resolve_path(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut result = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Prefix(prefix) => result.push(prefix.as_os_str()),
            Component::RootDir => result.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                let can_pop = matches!(result.components().next_back(), Some(Component::Normal(_)));
                if can_pop {
                    result.pop();
                } else if !result.has_root() {
                    result.push("..");
                }
            }
            Component::Normal(part) => result.push(part),
        }
    }
    if result.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        result
    }
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    resolve_path(left) == resolve_path(right)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Upstream `getAgentDir()`: `PI_CODING_AGENT_DIR` or `~/.pi/agent`.
pub fn get_agent_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR") {
        let path = PathBuf::from(dir);
        if !path.as_os_str().is_empty() {
            return resolve_path(&path);
        }
    }
    home_dir()
        .map(|home| home.join(".pi").join("agent"))
        .unwrap_or_else(|| PathBuf::from(".pi/agent"))
}

/// `<agent-dir>/sessions/--<encoded cwd>--` (upstream `getDefaultSessionDir`).
pub fn get_default_session_dir(cwd: impl AsRef<Path>) -> PathBuf {
    let resolved = resolve_path(cwd.as_ref());
    let text = resolved.to_string_lossy();
    let safe = text
        .strip_prefix('/')
        .or_else(|| text.strip_prefix('\\'))
        .unwrap_or(&text)
        .replace(['/', '\\', ':'], "-");
    get_agent_dir().join("sessions").join(format!("--{safe}--"))
}

// ---------------------------------------------------------------------------
// Parsing and migrations
// ---------------------------------------------------------------------------

/// Split a JSONL string into parsed objects, skipping blanks and malformed
/// lines (upstream `parseSessionEntries`). A leading UTF-8 BOM is stripped.
pub fn parse_session_entries(content: &str) -> Vec<Value> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    content
        .split('\n')
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                None
            } else {
                serde_json::from_str::<Value>(line)
                    .ok()
                    .filter(Value::is_object)
            }
        })
        .collect()
}

/// Read and validate a session file. Empty/malformed files yield no entries;
/// a trailing unterminated valid record is repaired by appending a newline.
pub fn load_entries_from_file(path: impl AsRef<Path>) -> Result<Vec<Value>, String> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(path).map_err(io_err)?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let entries = parse_session_entries(text);
    let Some(first) = entries.first() else {
        return Ok(Vec::new());
    };
    if !is_system_entry(first) || field_str(first, "id").is_none() {
        return Ok(Vec::new());
    }
    if !text.ends_with('\n') {
        OpenOptions::new()
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(b"\n"))
            .map_err(io_err)?;
    }
    Ok(entries)
}

/// Read only the first megabyte looking for a session header. Discovery must
/// not repair files or choke on a large corrupt prefix.
fn read_session_header(path: impl AsRef<Path>) -> Option<Value> {
    let mut file = File::open(path).ok()?;
    let mut buffer = vec![0u8; SESSION_HEADER_SCAN_LIMIT as usize];
    let read = file.read(&mut buffer).ok()?;
    let text = String::from_utf8_lossy(&buffer[..read]);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if !value.is_object() {
            continue;
        }
        if is_system_entry(&value) {
            field_str(&value, "id")?;
            return Some(value);
        }
        return None;
    }
    None
}

/// Migrate v1 -> v2 -> v3 in place. Returns true when anything changed.
pub fn migrate_session_entries(entries: &mut [Value]) -> bool {
    let mut version = 1u32;
    for entry in entries.iter() {
        if is_system_entry(entry) {
            version = entry
                .get("version")
                .and_then(Value::as_u64)
                .map_or(1, |v| v as u32);
            break;
        }
    }
    if version >= CURRENT_SESSION_VERSION {
        return false;
    }

    if version < 2 {
        let mut ids = HashSet::new();
        let mut previous: Option<String> = None;
        let mut assigned: Vec<Option<String>> = Vec::with_capacity(entries.len());
        for entry in entries.iter_mut() {
            if is_system_entry(entry) {
                entry
                    .as_object_mut()
                    .map(|object| object.insert("version".to_owned(), json!(2)));
                assigned.push(None);
                continue;
            }
            let id = generate_short_id(&ids);
            ids.insert(id.clone());
            if let Some(object) = entry.as_object_mut() {
                object.insert("id".to_owned(), json!(id.clone()));
                object.insert(
                    "parentId".to_owned(),
                    previous.as_ref().map_or(Value::Null, |p| json!(p)),
                );
            }
            previous = Some(id.clone());
            assigned.push(Some(id));
        }
        // Compaction's firstKeptEntryIndex points at a position in the old
        // linear file; resolve it after ids have been assigned.
        for (index, entry) in entries.iter_mut().enumerate() {
            if field_str(entry, "type") != Some("compaction") {
                continue;
            }
            let old_index = entry
                .get("firstKeptEntryIndex")
                .and_then(Value::as_u64)
                .map(|value| value as usize);
            if let Some(object) = entry.as_object_mut() {
                object.remove("firstKeptEntryIndex");
                if let Some(target) = old_index
                    .and_then(|i| assigned.get(i))
                    .and_then(Option::as_ref)
                {
                    object.insert("firstKeptEntryId".to_owned(), json!(target));
                }
            }
            let _ = index;
        }
    }

    if version < 3 {
        for entry in entries.iter_mut() {
            if is_system_entry(entry) {
                if let Some(object) = entry.as_object_mut() {
                    object.insert("version".to_owned(), json!(3));
                }
                continue;
            }
            if field_str(entry, "type") == Some("message")
                && let Some(message) = entry.get_mut("message").and_then(Value::as_object_mut)
                && message.get("role").and_then(Value::as_str) == Some("hookMessage")
            {
                message.insert("role".to_owned(), json!("custom"));
            }
        }
    }
    true
}

fn entry_from_raw(value: &Value) -> Option<SessionEntry> {
    let kind = field_str(value, "type")?;
    if kind == "session" {
        return None;
    }
    serde_json::from_value::<SessionEntry>(value.clone()).ok()
}

/// Latest compaction entry on a list of entries, upstream
/// `getLatestCompactionEntry()`.
pub fn get_latest_compaction_entry(entries: &[SessionEntry]) -> Option<&SessionEntry> {
    entries
        .iter()
        .rev()
        .find(|entry| matches!(entry, SessionEntry::Compaction { .. }))
}

// ---------------------------------------------------------------------------
// Context projection
// ---------------------------------------------------------------------------

fn build_entry_index(entries: &[SessionEntry]) -> HashMap<&str, usize> {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.id(), index))
        .collect()
}

fn build_session_path<'a>(
    entries: &'a [SessionEntry],
    leaf_id: Option<&str>,
) -> Vec<&'a SessionEntry> {
    let index = build_entry_index(entries);
    let mut current = match leaf_id {
        Some(id) => index
            .get(id)
            .copied()
            .or_else(|| entries.len().checked_sub(1)),
        None => entries.len().checked_sub(1),
    };
    let mut path = Vec::new();
    while let Some(position) = current {
        let entry = &entries[position];
        path.push(entry);
        current = entry
            .parent_id()
            .and_then(|parent| index.get(parent).copied());
    }
    path.reverse();
    path
}

fn get_context_settings(path: &[&SessionEntry]) -> (String, Option<ModelRef>) {
    let mut thinking_level = "off".to_owned();
    let mut model = None;
    for entry in path {
        match entry {
            SessionEntry::ThinkingLevelChange {
                thinking_level: level,
                ..
            } => {
                thinking_level.clone_from(level);
            }
            SessionEntry::ModelChange {
                provider, model_id, ..
            } => {
                model = Some(ModelRef {
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                });
            }
            SessionEntry::Message { message, .. }
                if message_role(message) == Some("assistant")
                    && let (Some(provider), Some(model_id)) =
                        (field_str(message, "provider"), field_str(message, "model")) =>
            {
                model = Some(ModelRef {
                    provider: provider.to_owned(),
                    model_id: model_id.to_owned(),
                });
            }
            _ => {}
        }
    }
    (thinking_level, model)
}

/// Resolve messages contributed by one entry.
pub fn session_entry_to_context_messages(entry: &SessionEntry) -> Vec<Value> {
    match entry {
        SessionEntry::Message { message, .. } => {
            let mut message = message.clone();
            let role = message_role(&message).unwrap_or("");
            let missing_content = message.get("content").is_none_or(Value::is_null);
            if missing_content && matches!(role, "system" | "user" | "assistant" | "toolResult") {
                let replacement = if role == "system" {
                    Value::String(String::new())
                } else {
                    json!([])
                };
                if let Some(object) = message.as_object_mut() {
                    object.insert("content".to_owned(), replacement);
                }
            }
            vec![message]
        }
        SessionEntry::CustomMessage {
            custom_type,
            content,
            display,
            details,
            timestamp,
            ..
        } => {
            let mut object = Map::new();
            object.insert("role".to_owned(), json!("custom"));
            object.insert("customType".to_owned(), json!(custom_type));
            object.insert("content".to_owned(), content.clone());
            object.insert("display".to_owned(), json!(display));
            if let Some(details) = details {
                object.insert("details".to_owned(), details.clone());
            }
            object.insert(
                "timestamp".to_owned(),
                json!(parse_timestamp_millis(timestamp)),
            );
            vec![Value::Object(object)]
        }
        SessionEntry::BranchSummary {
            summary,
            from_id,
            timestamp,
            ..
        } if !summary.is_empty() => {
            vec![json!({
                "role": "branchSummary",
                "summary": summary,
                "fromId": from_id,
                "timestamp": parse_timestamp_millis(timestamp),
            })]
        }
        SessionEntry::Compaction {
            summary,
            tokens_before,
            timestamp,
            system_message,
            ..
        } => {
            let mut messages = Vec::new();
            if let Some(system_message) = system_message {
                messages.push(system_message.clone());
            }
            messages.push(json!({
                "role": "compactionSummary",
                "summary": summary,
                "tokensBefore": tokens_before,
                "timestamp": parse_timestamp_millis(timestamp),
            }));
            messages
        }
        _ => Vec::new(),
    }
}

/// Apply the latest context edit for an entry to its projected messages.
pub fn project_context_entry(entry: &SessionEntry, edit: Option<&SessionEntry>) -> Vec<Value> {
    let mut messages = session_entry_to_context_messages(entry);
    let Some(SessionEntry::ContextEdit { replacement, .. }) = edit else {
        return messages;
    };
    if replacement.is_null() {
        return Vec::new();
    }
    let raw_content = replacement.get("content");
    for message in &mut messages {
        let Some(role) = message_role(message) else {
            continue;
        };
        if !matches!(role, "user" | "assistant" | "toolResult" | "custom") {
            continue;
        }
        let content = match raw_content {
            Some(Value::String(text)) if matches!(role, "assistant" | "toolResult") => {
                Value::Array(vec![json!({ "type": "text", "text": text })])
            }
            Some(other) => other.clone(),
            None => continue,
        };
        if let Some(object) = message.as_object_mut() {
            object.insert("content".to_owned(), content);
        }
    }
    messages
}

/// The compaction-aware entry list along the path to a leaf.
pub fn build_context_entries<'a>(
    entries: &'a [SessionEntry],
    leaf_id: Option<&str>,
) -> Vec<&'a SessionEntry> {
    let path = build_session_path(entries, leaf_id);
    // Upstream keeps overwriting `compaction` while walking the path, so the
    // latest compaction wins.
    let Some(compaction) = path
        .iter()
        .rev()
        .find(|entry| matches!(entry, SessionEntry::Compaction { .. }))
    else {
        return path;
    };
    let compaction: &'a SessionEntry = compaction;
    let compaction_id = compaction.id();
    let first_kept = match compaction {
        SessionEntry::Compaction {
            first_kept_entry_id,
            ..
        } => first_kept_entry_id.as_str(),
        _ => compaction_id,
    };
    let compaction_position = path
        .iter()
        .position(|entry| entry.id() == compaction_id)
        .unwrap_or(path.len());
    let mut context: Vec<&'a SessionEntry> = vec![compaction];
    let mut found_first_kept = false;
    for &entry in path.iter().take(compaction_position) {
        if entry.id() == first_kept {
            found_first_kept = true;
        }
        if found_first_kept {
            let is_system = matches!(entry, SessionEntry::Message { message, .. } if message_role(message) == Some("system"));
            if !is_system {
                context.push(entry);
            }
        }
    }
    context.extend(path.iter().skip(compaction_position + 1).copied());
    context
}

/// Provenance-preserving, compaction-aware projection.
pub fn build_session_projection(
    entries: &[SessionEntry],
    leaf_id: Option<&str>,
) -> SessionProjection {
    let path = build_session_path(entries, leaf_id);
    let (thinking_level, model) = get_context_settings(&path);
    let context_entries = build_context_entries(entries, leaf_id);
    let mut edits: HashMap<&str, &SessionEntry> = HashMap::new();
    for &entry in &context_entries {
        if let SessionEntry::ContextEdit { target_id, .. } = entry {
            edits.insert(target_id.as_str(), entry);
        }
    }
    let projected: Vec<ProjectedSessionEntry> = context_entries
        .iter()
        .enumerate()
        .map(|(index, &source_entry)| {
            let messages = if matches!(source_entry, SessionEntry::Compaction { .. }) && index > 0 {
                Vec::new()
            } else {
                project_context_entry(source_entry, edits.get(source_entry.id()).copied())
            };
            ProjectedSessionEntry {
                source_entry: source_entry.clone(),
                messages,
            }
        })
        .collect();
    let messages = projected
        .iter()
        .flat_map(|entry| entry.messages.clone())
        .collect();
    SessionProjection {
        entries: projected,
        messages,
        thinking_level,
        model,
    }
}

/// Build the finalized model context from a list of entries.
pub fn build_session_context(entries: &[SessionEntry], leaf_id: Option<&str>) -> SessionContext {
    let projection = build_session_projection(entries, leaf_id);
    SessionContext {
        messages: projection.messages,
        thinking_level: projection.thinking_level,
        model: projection.model,
    }
}

/// Replay system messages into the compacted system message used by upstream
/// `appendCompaction()`.
fn get_current_system_message(messages: &[Value]) -> Option<Value> {
    let mut content: Vec<String> = Vec::new();
    let mut sections = Map::new();
    let mut tools = Map::new();
    let mut timestamp = None;
    for message in messages {
        if message_role(message) != Some("system") {
            continue;
        }
        if timestamp.is_none() {
            timestamp = message.get("timestamp").and_then(Value::as_i64).or(Some(0));
        }
        if let Some(text) = message.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            content.push(text.to_owned());
        }
        if let Some(patch) = message.get("sections").and_then(Value::as_object) {
            for (key, value) in patch {
                if value.is_null() {
                    sections.remove(key);
                } else {
                    sections.insert(key.clone(), value.clone());
                }
            }
        }
        for tool in message
            .get("toolsRemoved")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(name) = tool.get("name").and_then(Value::as_str) {
                tools.remove(name);
            }
        }
        for tool in message
            .get("toolsAdded")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(name) = tool.get("name").and_then(Value::as_str) {
                tools.insert(name.to_owned(), tool.clone());
            }
        }
    }
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    let mut object = Map::new();
    object.insert("role".to_owned(), json!("system"));
    object.insert("content".to_owned(), json!(content.join("\n\n")));
    if !sections.is_empty() {
        object.insert("sections".to_owned(), Value::Object(sections));
    }
    if !tools.is_empty() {
        object.insert(
            "toolsAdded".to_owned(),
            Value::Array(tools.into_values().collect()),
        );
    }
    object.insert("timestamp".to_owned(), json!(timestamp.unwrap_or(0)));
    Some(Value::Object(object))
}

fn extract_text_content(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| {
                if field_str(block, "type") == Some("text") {
                    field_str(block, "text").map(str::to_owned)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn file_mtime_millis(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn build_session_info(path: &Path) -> Option<SessionInfo> {
    // Discovery parses in memory: listing sessions must not repair files.
    let bytes = fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let values = parse_session_entries(&text);
    let mut header: Option<Value> = None;
    let mut name: Option<String> = None;
    let mut message_count = 0usize;
    let mut last_activity: i64 = 0;
    let mut first_message = String::new();
    let mut all_messages: Vec<String> = Vec::new();
    for value in values {
        let kind = field_str(&value, "type").unwrap_or("").to_owned();
        if header.is_none() {
            if kind != "session" {
                return None;
            }
            header = Some(value);
            continue;
        }
        if kind == "session_info" {
            name = value
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned);
        }
        if kind != "message" {
            continue;
        }
        message_count += 1;
        let Some(message) = value.get("message") else {
            continue;
        };
        let role = message_role(message).unwrap_or("");
        if role != "user" && role != "assistant" {
            continue;
        }
        let activity = message
            .get("timestamp")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| {
                parse_timestamp_millis(field_str(&value, "timestamp").unwrap_or(""))
            });
        if activity > last_activity {
            last_activity = activity;
        }
        let text = extract_text_content(message);
        if text.is_empty() {
            continue;
        }
        all_messages.push(text.clone());
        if first_message.is_empty() && role == "user" {
            first_message = text;
        }
    }
    let header = header?;
    let created = field_str(&header, "timestamp").unwrap_or("").to_owned();
    let created_millis = parse_timestamp_millis(&created);
    let modified_millis = if last_activity > 0 {
        last_activity
    } else if created_millis != 0 {
        created_millis
    } else {
        file_mtime_millis(path)
    };
    let parent_session_path = field_str(&header, "parentSession").map(PathBuf::from);
    Some(SessionInfo {
        path: path.to_path_buf(),
        id: field_str(&header, "id").unwrap_or("").to_owned(),
        cwd: field_str(&header, "cwd").unwrap_or("").to_owned(),
        name,
        parent_session_path,
        modified: format_timestamp(modified_millis),
        created,
        created_millis,
        modified_millis,
        message_count,
        first_message: if first_message.is_empty() {
            "(no messages)".to_owned()
        } else {
            first_message
        },
        all_messages_text: all_messages.join(" "),
    })
}

/// List sessions in a directory, newest first (upstream `listSessionsFromDir`).
pub fn list_sessions(dir: impl AsRef<Path>) -> Result<Vec<SessionInfo>, String> {
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                files.push(path);
            }
        }
    }
    files.sort_by(|left, right| right.file_name().cmp(&left.file_name()));
    let mut sessions: Vec<SessionInfo> = files
        .into_iter()
        .filter_map(|path| build_session_info(&path))
        .collect();
    sessions.sort_by(|left, right| {
        right
            .modified_millis
            .cmp(&left.modified_millis)
            .then_with(|| right.path.cmp(&left.path))
    });
    Ok(sessions)
}

/// Find the most recently modified valid session, optionally filtered by cwd.
pub fn find_most_recent_session(dir: impl AsRef<Path>, cwd: Option<&Path>) -> Option<PathBuf> {
    let mut candidates: Vec<(PathBuf, i64)> = Vec::new();
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            let mtime = file_mtime_millis(&path);
            candidates.push((path, mtime));
        }
    }
    candidates.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| right.0.cmp(&left.0)));
    for (path, _) in candidates {
        let Some(header) = read_session_header(&path) else {
            continue;
        };
        let header_cwd = field_str(&header, "cwd").unwrap_or("");
        match cwd {
            None => return Some(path),
            Some(expected)
                if !header_cwd.is_empty() && paths_equal(Path::new(header_cwd), expected) =>
            {
                return Some(path);
            }
            Some(_) => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// SessionManager
// ---------------------------------------------------------------------------

/// Append-only JSONL v3 session store.
#[derive(Debug)]
pub struct SessionManager {
    /// Session file, if persisted.
    pub path: Option<PathBuf>,
    /// Typed body entries in file order (header excluded).
    pub entries: Vec<SessionEntry>,
    header: SessionHeader,
    session_id: String,
    /// Raw records, including the header and any unknown upstream entry types.
    raw_entries: Vec<Value>,
    by_id: HashMap<String, usize>,
    leaf_id: Option<String>,
    labels: indexmap::IndexMap<String, String>,
    label_timestamps: HashMap<String, String>,
    cwd: String,
    session_dir: PathBuf,
    persist: bool,
    flushed: bool,
    last_good_offset: u64,
}

impl SessionManager {
    fn empty(cwd: String, session_dir: PathBuf, path: Option<PathBuf>, persist: bool) -> Self {
        Self {
            path,
            entries: Vec::new(),
            header: SessionHeader {
                version: Some(CURRENT_SESSION_VERSION),
                id: String::new(),
                timestamp: String::new(),
                cwd: cwd.clone(),
                parent_session: None,
            },
            session_id: String::new(),
            raw_entries: Vec::new(),
            by_id: HashMap::new(),
            leaf_id: None,
            labels: indexmap::IndexMap::new(),
            label_timestamps: HashMap::new(),
            cwd,
            session_dir,
            persist,
            flushed: false,
            last_good_offset: 0,
        }
    }

    /// In-memory session in the current working directory.
    pub fn in_memory() -> Self {
        Self::in_memory_at(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    /// In-memory session in an explicit working directory.
    pub fn in_memory_at(cwd: impl AsRef<Path>) -> Self {
        let cwd = resolve_path(cwd.as_ref()).to_string_lossy().into_owned();
        let mut manager = Self::empty(cwd, PathBuf::new(), None, false);
        let _ = manager.new_session(None, None);
        manager
    }

    /// In-memory session seeded with entries (with or without a header).
    pub fn in_memory_with_entries(
        cwd: impl AsRef<Path>,
        id: Option<&str>,
        entries: Vec<SessionEntry>,
    ) -> Result<Self, String> {
        let cwd = resolve_path(cwd.as_ref()).to_string_lossy().into_owned();
        let mut manager = Self::empty(cwd, PathBuf::new(), None, false);
        let raw: Vec<Value> = entries
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?;
        manager.load_raw(raw, id, None)?;
        Ok(manager)
    }

    /// Open a session file. Empty files are initialized; files with content
    /// but no valid header are rejected without modification.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        Self::open_with(path, None, None)
    }

    /// Upstream `SessionManager.open()` with an explicit session directory and
    /// optional cwd override.
    pub fn open_with(
        path: impl AsRef<Path>,
        session_dir: Option<PathBuf>,
        cwd_override: Option<&Path>,
    ) -> Result<Self, String> {
        let path = resolve_path(path.as_ref());
        let dir = session_dir
            .map(|dir| resolve_path(&dir))
            .unwrap_or_else(|| {
                path.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."))
            });
        let raw = if path.exists() {
            load_entries_from_file(&path)?
        } else {
            Vec::new()
        };
        let header = raw.iter().find(|value| is_system_entry(value));
        let cwd = cwd_override
            .map(|cwd| resolve_path(cwd).to_string_lossy().into_owned())
            .or_else(|| header.and_then(|value| field_str(value, "cwd").map(str::to_owned)))
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .to_string_lossy()
                    .into_owned()
            });
        let mut manager = Self::empty(cwd, dir, Some(path.clone()), true);
        if path.exists() {
            if raw.is_empty() {
                if fs::metadata(&path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0)
                    > 0
                {
                    return Err(format!(
                        "Session file is not a valid pi session: {}",
                        path.display()
                    ));
                }
                manager.new_session(None, None)?;
                // `newSession()` picks a generated path; the explicit path of
                // the empty file wins, as in upstream `_setSessionFile()`.
                manager.path = Some(path.clone());
                manager.flushed = true;
                manager.rewrite_file()?;
            } else {
                manager.load_raw(raw, None, None)?;
                manager.flushed = true;
                manager.last_good_offset = fs::metadata(&path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0);
            }
        } else {
            manager.new_session(None, None)?;
        }
        Ok(manager)
    }

    /// Create a persisted session in the default cwd-derived session directory.
    pub fn create(cwd: impl AsRef<Path>, id: Option<&str>) -> Result<Self, String> {
        let cwd_path = resolve_path(cwd.as_ref());
        let dir = get_default_session_dir(&cwd_path);
        Self::create_in(&cwd_path, dir, id)
    }

    /// Create a persisted session in an explicit directory.
    pub fn create_in(
        cwd: impl AsRef<Path>,
        session_dir: impl AsRef<Path>,
        id: Option<&str>,
    ) -> Result<Self, String> {
        let cwd = resolve_path(cwd.as_ref()).to_string_lossy().into_owned();
        let dir = resolve_path(session_dir.as_ref());
        fs::create_dir_all(&dir).map_err(io_err)?;
        let mut manager = Self::empty(cwd, dir, None, true);
        manager.new_session(id, None)?;
        Ok(manager)
    }

    /// Fork a source session file into a new session in the target cwd.
    pub fn fork_from(
        source_path: impl AsRef<Path>,
        target_cwd: impl AsRef<Path>,
        session_dir: Option<PathBuf>,
        id: Option<&str>,
    ) -> Result<Self, String> {
        let source = resolve_path(source_path.as_ref());
        let target = resolve_path(target_cwd.as_ref());
        let source_entries = load_entries_from_file(&source)?;
        if source_entries.is_empty() || !source_entries.iter().any(is_system_entry) {
            return Err(format!(
                "Cannot fork: source session file is empty or invalid: {}",
                source.display()
            ));
        }
        let dir = session_dir
            .map(|dir| resolve_path(&dir))
            .unwrap_or_else(|| get_default_session_dir(&target));
        fs::create_dir_all(&dir).map_err(io_err)?;
        if let Some(id) = id {
            assert_valid_session_id(id)?;
        }
        let session_id = id.map(str::to_owned).unwrap_or_else(create_session_id);
        let timestamp = timestamp_now();
        let header = SessionHeader {
            version: Some(CURRENT_SESSION_VERSION),
            id: session_id.clone(),
            timestamp: timestamp.clone(),
            cwd: target.to_string_lossy().into_owned(),
            parent_session: Some(source.to_string_lossy().into_owned()),
        };
        let file_timestamp = timestamp.replace([':', '.'], "-");
        let path = dir.join(format!("{file_timestamp}_{session_id}.jsonl"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io_err)?;
        serde_json::to_writer(&mut file, &header).map_err(|err| err.to_string())?;
        file.write_all(b"\n").map_err(io_err)?;
        for entry in source_entries {
            if !is_system_entry(&entry) {
                serde_json::to_writer(&mut file, &entry).map_err(|err| err.to_string())?;
                file.write_all(b"\n").map_err(io_err)?;
            }
        }
        file.flush().map_err(io_err)?;
        drop(file);
        Self::open_with(&path, Some(dir), Some(&target))
    }

    /// Reset the session identity and entries, keeping cwd and directory.
    pub fn new_session(
        &mut self,
        id: Option<&str>,
        parent_session: Option<&str>,
    ) -> Result<(), String> {
        if let Some(id) = id {
            assert_valid_session_id(id)?;
        }
        let session_id = id.map(str::to_owned).unwrap_or_else(create_session_id);
        let timestamp = timestamp_now();
        let header = SessionHeader {
            version: Some(CURRENT_SESSION_VERSION),
            id: session_id.clone(),
            timestamp,
            cwd: self.cwd.clone(),
            parent_session: parent_session.map(str::to_owned),
        };
        self.header = header.clone();
        self.session_id = session_id.clone();
        self.raw_entries = vec![serde_json::to_value(&header).map_err(|err| err.to_string())?];
        self.entries.clear();
        self.by_id.clear();
        self.labels.clear();
        self.label_timestamps.clear();
        self.leaf_id = None;
        self.flushed = false;
        self.last_good_offset = 0;
        if self.persist {
            fs::create_dir_all(&self.session_dir).map_err(io_err)?;
            let file_timestamp = header.timestamp.replace([':', '.'], "-");
            self.path = Some(
                self.session_dir
                    .join(format!("{file_timestamp}_{session_id}.jsonl")),
            );
        }
        Ok(())
    }

    fn load_raw(
        &mut self,
        mut raw: Vec<Value>,
        id: Option<&str>,
        parent_session: Option<&str>,
    ) -> Result<(), String> {
        // Upstream only migrates when a header is present. Headerless entries
        // are adopted verbatim as current-version entries.
        let has_header = raw.iter().any(is_system_entry);
        let migrated = has_header && migrate_session_entries(&mut raw);
        let header = raw
            .iter()
            .find(|value| is_system_entry(value))
            .and_then(|value| serde_json::from_value::<SessionHeader>(value.clone()).ok());
        match header {
            Some(header) => {
                self.session_id = header.id.clone();
                self.header = header;
                self.raw_entries = raw;
            }
            None => {
                self.new_session(id, parent_session)?;
                self.raw_entries
                    .extend(raw.into_iter().filter(|value| !is_system_entry(value)));
            }
        }
        self.rebuild_index();
        if migrated {
            self.rewrite_file()?;
        }
        Ok(())
    }

    fn rebuild_index(&mut self) {
        self.entries.clear();
        self.by_id.clear();
        self.labels.clear();
        self.label_timestamps.clear();
        self.leaf_id = None;
        for value in &self.raw_entries {
            let Some(entry) = entry_from_raw(value) else {
                continue;
            };
            let id = entry.id().to_owned();
            self.by_id.insert(id.clone(), self.entries.len());
            self.leaf_id = Some(id);
            if let SessionEntry::Label {
                target_id,
                label,
                timestamp,
                ..
            } = &entry
            {
                if let Some(label) = label {
                    self.labels.insert(target_id.clone(), label.clone());
                    self.label_timestamps
                        .insert(target_id.clone(), timestamp.clone());
                } else {
                    self.labels.shift_remove(target_id);
                    self.label_timestamps.remove(target_id);
                }
            }
            self.entries.push(entry);
        }
    }

    fn rewrite_file(&mut self) -> Result<(), String> {
        let (Some(path), true) = (self.path.as_ref(), self.persist) else {
            return Ok(());
        };
        let path = path.clone();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io_err)?;
        }
        let mut file = File::create(&path).map_err(io_err)?;
        for value in &self.raw_entries {
            serde_json::to_writer(&mut file, value).map_err(|err| err.to_string())?;
            file.write_all(b"\n").map_err(io_err)?;
        }
        file.flush().map_err(io_err)?;
        self.last_good_offset = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        Ok(())
    }

    fn has_conversation(&self) -> bool {
        self.entries.iter().any(|entry| {
            matches!(entry, SessionEntry::Message { message, .. }
                if matches!(message_role(message), Some("user") | Some("assistant")))
        })
    }

    fn flush_new_file(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io_err)?;
        let result = (|| -> Result<(), String> {
            for value in &self.raw_entries {
                serde_json::to_writer(&mut file, value).map_err(|err| err.to_string())?;
                file.write_all(b"\n").map_err(io_err)?;
            }
            file.flush().map_err(io_err)
        })();
        if let Err(err) = result {
            let _ = fs::remove_file(&path);
            return Err(err);
        }
        self.last_good_offset = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        Ok(())
    }

    /// Append one serialized entry. A failed write rolls the file back to the
    /// last known-good offset, then the caller rolls back in-memory state.
    fn persist_entry(&mut self, entry: &SessionEntry) -> Result<(), String> {
        if !self.persist || self.path.is_none() {
            return Ok(());
        }
        if !self.flushed {
            if !self.has_conversation() {
                return Ok(());
            }
            self.flush_new_file()?;
            self.flushed = true;
            return Ok(());
        }
        let path = self.path.clone().unwrap_or_default();
        let line = format!(
            "{}\n",
            serde_json::to_string(entry).map_err(|err| err.to_string())?
        );
        let good_offset = self.last_good_offset;
        let result = (|| -> Result<(), String> {
            let mut file = OpenOptions::new()
                .append(true)
                .open(&path)
                .map_err(io_err)?;
            file.write_all(line.as_bytes()).map_err(io_err)?;
            file.flush().map_err(io_err)
        })();
        if result.is_err() {
            if let Ok(file) = OpenOptions::new().write(true).open(&path) {
                let _ = file.set_len(good_offset);
            }
            return result;
        }
        self.last_good_offset = good_offset.saturating_add(line.len() as u64);
        Ok(())
    }

    /// Append an already-built entry as a child of the current leaf. Missing
    /// ids/timestamps are filled in; the parent is always the current leaf.
    pub fn append_entry(&mut self, mut entry: SessionEntry) -> Result<String, String> {
        if entry.id().is_empty() {
            let used: HashSet<String> = self.by_id.keys().cloned().collect();
            entry.set_id(generate_short_id(&used));
        }
        if entry.timestamp().is_empty() {
            entry.set_timestamp(timestamp_now());
        }
        entry.set_parent_id(self.leaf_id.clone());
        let id = entry.id().to_owned();
        let previous_leaf = self.leaf_id.clone();
        let raw = serde_json::to_value(&entry).map_err(|err| err.to_string())?;
        self.entries.push(entry.clone());
        self.by_id.insert(id.clone(), self.entries.len() - 1);
        self.leaf_id = Some(id.clone());
        let previous_label = match &entry {
            SessionEntry::Label { target_id, .. } => Some((
                target_id.clone(),
                self.labels.get(target_id).cloned(),
                self.label_timestamps.get(target_id).cloned(),
            )),
            _ => None,
        };
        self.raw_entries.push(raw);
        self.apply_label_entry(&entry);
        if let Err(err) = self.persist_entry(&entry) {
            self.raw_entries.pop();
            self.entries.pop();
            self.by_id.remove(&id);
            self.leaf_id = previous_leaf;
            if let Some((target_id, label, timestamp)) = previous_label {
                match label {
                    Some(label) => {
                        self.labels.insert(target_id.clone(), label);
                    }
                    None => {
                        self.labels.shift_remove(&target_id);
                    }
                }
                match timestamp {
                    Some(timestamp) => {
                        self.label_timestamps.insert(target_id, timestamp);
                    }
                    None => {
                        self.label_timestamps.remove(&target_id);
                    }
                }
            }
            return Err(err);
        }
        Ok(id)
    }

    fn apply_label_entry(&mut self, entry: &SessionEntry) {
        if let SessionEntry::Label {
            target_id,
            label,
            timestamp,
            ..
        } = entry
        {
            if let Some(label) = label {
                self.labels.insert(target_id.clone(), label.clone());
                self.label_timestamps
                    .insert(target_id.clone(), timestamp.clone());
            } else {
                self.labels.shift_remove(target_id);
                self.label_timestamps.remove(target_id);
            }
        }
    }

    pub fn append_message(&mut self, message: Value) -> Result<String, String> {
        self.append_entry(SessionEntry::Message {
            id: String::new(),
            parent_id: None,
            timestamp: String::new(),
            message,
        })
    }

    pub fn append_thinking_level_change(&mut self, thinking_level: &str) -> Result<String, String> {
        self.append_entry(SessionEntry::ThinkingLevelChange {
            id: String::new(),
            parent_id: None,
            timestamp: String::new(),
            thinking_level: thinking_level.to_owned(),
        })
    }

    pub fn append_model_change(
        &mut self,
        provider: &str,
        model_id: &str,
    ) -> Result<String, String> {
        self.append_entry(SessionEntry::ModelChange {
            id: String::new(),
            parent_id: None,
            timestamp: String::new(),
            provider: provider.to_owned(),
            model_id: model_id.to_owned(),
        })
    }

    pub fn append_usage(
        &mut self,
        kind: &str,
        provider: &str,
        model: &str,
        usage: Value,
        note: Option<&str>,
    ) -> Result<String, String> {
        self.append_entry(SessionEntry::Usage {
            id: String::new(),
            parent_id: None,
            timestamp: String::new(),
            kind: kind.to_owned(),
            provider: provider.to_owned(),
            model: model.to_owned(),
            usage,
            note: note.map(str::to_owned),
        })
    }

    pub fn append_compaction(
        &mut self,
        summary: &str,
        first_kept_entry_id: Option<&str>,
        tokens_before: u64,
    ) -> Result<String, String> {
        self.append_compaction_with_options(
            summary,
            first_kept_entry_id,
            tokens_before,
            CompactionExtra::default(),
        )
    }

    pub fn append_compaction_with_options(
        &mut self,
        summary: &str,
        first_kept_entry_id: Option<&str>,
        tokens_before: u64,
        mut extra: CompactionExtra,
    ) -> Result<String, String> {
        let id = generate_short_id(&self.by_id.keys().cloned().collect());
        let first_kept = first_kept_entry_id
            .map(str::to_owned)
            .unwrap_or_else(|| id.clone());
        let timestamp = timestamp_now();
        if extra.system_message.is_none() {
            let messages = self.build_session_projection().messages;
            extra.system_message = get_current_system_message(&messages);
        }
        if let Some(system_message) = extra.system_message.as_mut().and_then(Value::as_object_mut) {
            system_message.insert(
                "timestamp".to_owned(),
                json!(parse_timestamp_millis(&timestamp)),
            );
        }
        self.append_entry(SessionEntry::Compaction {
            id,
            parent_id: None,
            timestamp,
            summary: summary.to_owned(),
            first_kept_entry_id: first_kept,
            tokens_before,
            details: extra.details,
            usage: extra.usage,
            from_hook: extra.from_hook,
            system_message: extra.system_message,
        })
    }

    pub fn append_custom_entry(
        &mut self,
        custom_type: &str,
        data: Option<Value>,
    ) -> Result<String, String> {
        self.append_entry(SessionEntry::Custom {
            id: String::new(),
            parent_id: None,
            timestamp: String::new(),
            custom_type: custom_type.to_owned(),
            data,
        })
    }

    pub fn append_custom_message_entry(
        &mut self,
        custom_type: &str,
        content: Value,
        display: bool,
        details: Option<Value>,
    ) -> Result<String, String> {
        self.append_entry(SessionEntry::CustomMessage {
            id: String::new(),
            parent_id: None,
            timestamp: String::new(),
            custom_type: custom_type.to_owned(),
            content,
            display,
            details,
        })
    }

    pub fn append_label_change(
        &mut self,
        target_id: &str,
        label: Option<&str>,
    ) -> Result<String, String> {
        if !self.by_id.contains_key(target_id) {
            return Err(format!("Entry {target_id} not found"));
        }
        self.append_entry(SessionEntry::Label {
            id: String::new(),
            parent_id: None,
            timestamp: String::new(),
            target_id: target_id.to_owned(),
            label: label.map(str::to_owned),
        })
    }

    pub fn append_session_info(&mut self, name: &str) -> Result<String, String> {
        let sanitized = name
            .split(['\r', '\n'])
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        self.append_entry(SessionEntry::SessionInfoEntry {
            id: String::new(),
            parent_id: None,
            timestamp: String::new(),
            name: Some(sanitized.trim().to_owned()),
        })
    }

    pub fn append_context_edit(
        &mut self,
        target_id: &str,
        replacement: Value,
    ) -> Result<String, String> {
        if !replacement.is_null() {
            let content_ok = replacement
                .get("content")
                .map(|content| content.is_string() || content.is_array())
                .unwrap_or(false);
            if !content_ok {
                return Err(
                    "Context edit replacement must be null or contain string/array content"
                        .to_owned(),
                );
            }
        }
        let Some(target) = self
            .by_id
            .get(target_id)
            .copied()
            .map(|index| self.entries[index].clone())
        else {
            return Err(format!("Entry {target_id} not found"));
        };
        if !self
            .get_branch(None)
            .iter()
            .any(|entry| entry.id() == target_id)
        {
            return Err(format!("Entry {target_id} is not on the active branch"));
        }
        let editable = matches!(target, SessionEntry::CustomMessage { .. })
            || matches!(target, SessionEntry::Message { ref message, .. }
                if matches!(message_role(message), Some("user") | Some("assistant") | Some("toolResult")));
        if !editable {
            return Err(format!(
                "Entry {target_id} does not contribute editable model content"
            ));
        }
        let target_role = match &target {
            SessionEntry::Message { message, .. } => message_role(message).unwrap_or("").to_owned(),
            _ => "custom".to_owned(),
        };
        let normalized = if !replacement.is_null()
            && matches!(target_role.as_str(), "assistant" | "toolResult")
            && replacement.get("content").is_some_and(Value::is_string)
        {
            let text = replacement
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("");
            json!({ "content": [{ "type": "text", "text": text }] })
        } else {
            replacement
        };
        self.append_entry(SessionEntry::ContextEdit {
            id: String::new(),
            parent_id: None,
            timestamp: String::new(),
            target_id: target_id.to_owned(),
            replacement: normalized,
        })
    }

    // -----------------------------------------------------------------------
    // Accessors and tree traversal
    // -----------------------------------------------------------------------

    pub fn is_persisted(&self) -> bool {
        self.persist
    }

    pub fn get_cwd(&self) -> &str {
        &self.cwd
    }

    pub fn get_session_dir(&self) -> &Path {
        &self.session_dir
    }

    pub fn get_session_id(&self) -> &str {
        &self.session_id
    }

    pub fn get_session_file(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn get_header(&self) -> &SessionHeader {
        &self.header
    }

    pub fn get_entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    pub fn get_entry_count(&self) -> usize {
        self.by_id.len()
    }

    pub fn get_entry(&self, id: &str) -> Option<&SessionEntry> {
        self.by_id
            .get(id)
            .and_then(|index| self.entries.get(*index))
    }

    pub fn get_leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    pub fn get_leaf_entry(&self) -> Option<&SessionEntry> {
        self.leaf_id.as_deref().and_then(|id| self.get_entry(id))
    }

    pub fn get_children(&self, parent_id: &str) -> Vec<&SessionEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.parent_id() == Some(parent_id))
            .collect()
    }

    pub fn get_label(&self, id: &str) -> Option<&str> {
        self.labels.get(id).map(String::as_str)
    }

    pub fn get_label_timestamp(&self, id: &str) -> Option<&str> {
        self.label_timestamps.get(id).map(String::as_str)
    }

    /// Current leaf id (the tree "position").
    pub fn position(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    /// Move the position to an entry, or back before the root with `None`.
    pub fn set_position(&mut self, id: Option<&str>) -> Result<(), String> {
        match id {
            None => {
                self.leaf_id = None;
                Ok(())
            }
            Some(id) => self.branch(id),
        }
    }

    /// Path from root to `from_id` (or the current leaf).
    pub fn get_branch(&self, from_id: Option<&str>) -> Vec<&SessionEntry> {
        let start = from_id.or(self.leaf_id.as_deref());
        let mut path = Vec::new();
        let mut current = start.and_then(|id| self.get_entry(id));
        while let Some(entry) = current {
            path.push(entry);
            current = entry.parent_id().and_then(|parent| self.get_entry(parent));
        }
        path.reverse();
        path
    }

    pub fn get_tree(&self) -> Vec<SessionTreeNode> {
        let nodes: Vec<SessionTreeNode> = self
            .entries
            .iter()
            .map(|entry| SessionTreeNode {
                entry: entry.clone(),
                children: Vec::new(),
                label: self.labels.get(entry.id()).cloned(),
                label_timestamp: self.label_timestamps.get(entry.id()).cloned(),
            })
            .collect();
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        let mut roots = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            match entry.parent_id() {
                None => roots.push(index),
                Some(parent) if parent == entry.id() => roots.push(index),
                Some(parent) => match self.by_id.get(parent) {
                    Some(&parent_index) => children[parent_index].push(index),
                    None => roots.push(index),
                },
            }
        }
        roots
            .into_iter()
            .map(|root| build_tree_node(&nodes, &children, root))
            .collect()
    }

    pub fn build_context_entries(&self) -> Vec<&SessionEntry> {
        build_context_entries(&self.entries, self.leaf_id.as_deref())
    }

    pub fn build_session_projection(&self) -> SessionProjection {
        build_session_projection(&self.entries, self.leaf_id.as_deref())
    }

    pub fn build_session_context(&self) -> SessionContext {
        build_session_context(&self.entries, self.leaf_id.as_deref())
    }

    /// Latest `session_info` name, or `None` when explicitly cleared.
    pub fn get_session_name(&self) -> Option<&str> {
        // The latest session_info entry wins, including an explicit clear.
        self.entries
            .iter()
            .rev()
            .find(|entry| matches!(entry, SessionEntry::SessionInfoEntry { .. }))
            .and_then(|entry| match entry {
                SessionEntry::SessionInfoEntry { name, .. } => name.as_deref(),
                _ => None,
            })
            .map(str::trim)
            .filter(|name| !name.is_empty())
    }

    // -----------------------------------------------------------------------
    // Branching
    // -----------------------------------------------------------------------

    pub fn branch(&mut self, id: &str) -> Result<(), String> {
        if !self.by_id.contains_key(id) {
            return Err(format!("Entry {id} not found"));
        }
        self.leaf_id = Some(id.to_owned());
        Ok(())
    }

    /// Alias used by tree navigation: move the leaf to an existing entry.
    pub fn navigate_tree(&mut self, id: &str) -> Result<(), String> {
        self.branch(id)
    }

    /// Like `navigate_tree` but records a branch summary of the abandoned path.
    pub fn navigate_tree_with_summary(
        &mut self,
        id: &str,
        summary: &str,
        extra: BranchSummaryExtra,
    ) -> Result<String, String> {
        self.branch_with_summary(Some(id), summary, extra)
    }

    /// Move the leaf before the first entry; the next append becomes a root.
    pub fn reset(&mut self) {
        self.leaf_id = None;
    }

    pub fn reset_leaf(&mut self) {
        self.reset();
    }

    pub fn branch_with_summary(
        &mut self,
        branch_from_id: Option<&str>,
        summary: &str,
        extra: BranchSummaryExtra,
    ) -> Result<String, String> {
        if let Some(id) = branch_from_id
            && !self.by_id.contains_key(id)
        {
            return Err(format!("Entry {id} not found"));
        }
        let from_id = self.leaf_id.clone().unwrap_or_else(|| "root".to_owned());
        self.leaf_id = branch_from_id.map(str::to_owned);
        self.append_entry(SessionEntry::BranchSummary {
            id: String::new(),
            parent_id: None,
            timestamp: String::new(),
            from_id,
            summary: summary.to_owned(),
            details: extra.details,
            usage: extra.usage,
            from_hook: extra.from_hook,
        })
    }

    /// Extract the path to a leaf into a new session (upstream
    /// `createBranchedSession`). In-memory sessions return `None`.
    pub fn fork(&mut self, leaf_id: &str) -> Result<Option<PathBuf>, String> {
        let path: Vec<SessionEntry> = self
            .get_branch(Some(leaf_id))
            .into_iter()
            .cloned()
            .collect();
        if path.is_empty() {
            return Err(format!("Entry {leaf_id} not found"));
        }

        // Drop label entries and re-chain the remaining path. A compaction's
        // firstKeptEntryId may point at a dropped label, so remap it.
        let mut kept: Vec<SessionEntry> = Vec::new();
        let mut pending_label_ids: Vec<String> = Vec::new();
        let mut replacements: HashMap<String, String> = HashMap::new();
        let mut path_parent: Option<String> = None;
        for entry in path {
            if matches!(entry, SessionEntry::Label { .. }) {
                pending_label_ids.push(entry.id().to_owned());
                continue;
            }
            for label_id in pending_label_ids.drain(..) {
                replacements.insert(label_id, entry.id().to_owned());
            }
            let mut entry = entry;
            let entry_id = entry.id().to_owned();
            if let SessionEntry::Compaction {
                first_kept_entry_id,
                ..
            } = &mut entry
                && *first_kept_entry_id != entry_id
                && let Some(replacement) = replacements.get(first_kept_entry_id)
            {
                first_kept_entry_id.clone_from(replacement);
            }
            entry.set_parent_id(path_parent.clone());
            path_parent = Some(entry.id().to_owned());
            kept.push(entry);
        }

        let session_id = create_session_id();
        let timestamp = timestamp_now();
        let parent_session = if self.persist {
            self.path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
        } else {
            None
        };
        let header = SessionHeader {
            version: Some(CURRENT_SESSION_VERSION),
            id: session_id.clone(),
            timestamp: timestamp.clone(),
            cwd: self.cwd.clone(),
            parent_session,
        };

        let path_ids: HashSet<String> = kept.iter().map(|entry| entry.id().to_owned()).collect();
        let label_entries_to_write: Vec<(String, String, String)> = self
            .labels
            .iter()
            .filter_map(|(target_id, label)| {
                if path_ids.contains(target_id) {
                    self.label_timestamps
                        .get(target_id)
                        .map(|stamp| (target_id.clone(), label.clone(), stamp.clone()))
                } else {
                    None
                }
            })
            .collect();

        let mut used_ids: HashSet<String> = path_ids.clone();
        let mut label_entries: Vec<SessionEntry> = Vec::new();
        let mut parent = path_parent;
        for (target_id, label, label_timestamp) in label_entries_to_write {
            let id = generate_short_id(&used_ids);
            used_ids.insert(id.clone());
            let entry = SessionEntry::Label {
                id,
                parent_id: parent.clone(),
                timestamp: label_timestamp,
                target_id,
                label: Some(label),
            };
            parent = Some(entry.id().to_owned());
            label_entries.push(entry);
        }

        let mut all_entries = kept;
        all_entries.extend(label_entries);
        let mut raw = vec![serde_json::to_value(&header).map_err(|err| err.to_string())?];
        for entry in &all_entries {
            raw.push(serde_json::to_value(entry).map_err(|err| err.to_string())?);
        }

        self.header = header;
        self.session_id = session_id.clone();
        self.raw_entries = raw;
        self.entries = all_entries;
        self.rebuild_index();

        if self.persist {
            let file_timestamp = timestamp.replace([':', '.'], "-");
            let new_path = self
                .session_dir
                .join(format!("{file_timestamp}_{session_id}.jsonl"));
            self.path = Some(new_path.clone());
            if self.has_conversation() {
                self.rewrite_file()?;
                self.flushed = true;
            } else {
                self.flushed = false;
                self.last_good_offset = 0;
            }
            Ok(Some(new_path))
        } else {
            self.flushed = false;
            Ok(None)
        }
    }

    pub fn create_branched_session(&mut self, leaf_id: &str) -> Result<Option<PathBuf>, String> {
        self.fork(leaf_id)
    }

    // -----------------------------------------------------------------------
    // Discovery
    // -----------------------------------------------------------------------

    /// List sessions in a directory, newest first.
    pub fn list_sessions(dir: impl AsRef<Path>) -> Result<Vec<SessionInfo>, String> {
        list_sessions(dir)
    }

    /// List sessions for a cwd. Custom directories are filtered by header cwd.
    pub fn list(
        cwd: impl AsRef<Path>,
        session_dir: Option<PathBuf>,
    ) -> Result<Vec<SessionInfo>, String> {
        let cwd = resolve_path(cwd.as_ref());
        let (dir, filter_cwd) = match session_dir {
            Some(dir) => {
                let dir = resolve_path(&dir);
                let filter = dir != resolve_path(&get_default_session_dir(&cwd));
                (dir, filter)
            }
            None => (get_default_session_dir(&cwd), false),
        };
        let sessions = list_sessions(&dir)?;
        Ok(sessions
            .into_iter()
            .filter(|session| {
                !filter_cwd
                    || (!session.cwd.is_empty() && paths_equal(Path::new(&session.cwd), &cwd))
            })
            .collect())
    }

    pub fn list_all(dir: impl AsRef<Path>) -> Result<Vec<SessionInfo>, String> {
        list_sessions(dir)
    }

    pub fn find_most_recent_session(dir: impl AsRef<Path>, cwd: Option<&Path>) -> Option<PathBuf> {
        find_most_recent_session(dir, cwd)
    }

    pub fn find_by_id(dir: impl AsRef<Path>, id: &str) -> Option<PathBuf> {
        let entries = fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(header) = read_session_header(&path)
                && field_str(&header, "id") == Some(id)
            {
                return Some(path);
            }
        }
        None
    }

    pub fn continue_recent(
        cwd: impl AsRef<Path>,
        session_dir: Option<PathBuf>,
    ) -> Result<Self, String> {
        let cwd_path = resolve_path(cwd.as_ref());
        let (dir, filter_cwd) = match session_dir {
            Some(dir) => {
                let dir = resolve_path(&dir);
                let filter = dir != resolve_path(&get_default_session_dir(&cwd_path));
                (dir, filter)
            }
            None => (get_default_session_dir(&cwd_path), false),
        };
        if let Some(path) = find_most_recent_session(&dir, filter_cwd.then_some(cwd_path.as_path()))
        {
            Self::open_with(path, Some(dir), None)
        } else {
            Self::create_in(cwd_path, dir, None)
        }
    }
}

/// Build one tree node iteratively so deep histories cannot overflow the stack.
fn build_tree_node(
    nodes: &[SessionTreeNode],
    children: &[Vec<usize>],
    root: usize,
) -> SessionTreeNode {
    let mut stack = vec![(root, false)];
    let mut built: HashMap<usize, SessionTreeNode> = HashMap::new();
    while let Some((index, processed)) = stack.pop() {
        if processed {
            let mut node = nodes[index].clone();
            let mut kids: Vec<SessionTreeNode> = Vec::new();
            for child in &children[index] {
                if let Some(child_node) = built.remove(child) {
                    kids.push(child_node);
                }
            }
            kids.sort_by_key(|node| parse_timestamp_millis(node.entry.timestamp()));
            node.children = kids;
            built.insert(index, node);
        } else {
            stack.push((index, true));
            for child in children[index].iter().rev() {
                stack.push((*child, false));
            }
        }
    }
    built.remove(&root).unwrap_or_else(|| nodes[root].clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn user_msg(text: &str) -> Value {
        json!({ "role": "user", "content": [{ "type": "text", "text": text }], "timestamp": 1 })
    }

    fn assistant_msg(text: &str) -> Value {
        json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": text }],
            "api": "anthropic-messages",
            "provider": "anthropic",
            "model": "test",
            "usage": { "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
                       "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 } },
            "stopReason": "stop",
            "timestamp": 2,
        })
    }

    fn write(path: &Path, content: &str) {
        fs::write(path, content).unwrap();
    }

    fn lines(path: &Path) -> Vec<Value> {
        parse_session_entries(&fs::read_to_string(path).unwrap())
    }

    fn entry_ids<'a>(entries: &[&'a SessionEntry]) -> Vec<&'a str> {
        entries.iter().map(|entry| entry.id()).collect()
    }

    #[test]
    fn in_memory_starts_with_header_and_no_entries() {
        let session = SessionManager::in_memory();
        assert!(session.entries.is_empty());
        assert_eq!(session.position(), None);
        assert!(!session.is_persisted());
        assert_eq!(session.get_session_file(), None);
        assert_eq!(session.get_header().version, Some(3));
        assert_eq!(session.get_header().id, session.get_session_id());
        assert!(create_session_id().len() == 36);
    }

    #[test]
    fn generated_session_id_is_uuid_v7() {
        let id = create_session_id();
        let bytes = id.as_bytes();
        assert_eq!(bytes[14], b'7');
        assert!(matches!(bytes[19], b'8' | b'9' | b'a' | b'b'));
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn session_id_validation() {
        assert!(assert_valid_session_id("abc-123_def.456").is_ok());
        for invalid in ["", "-abc", "abc-", "_abc", "abc.", "a/b", "a b"] {
            assert!(assert_valid_session_id(invalid).is_err());
        }
    }

    #[test]
    fn append_chain_sets_parent_ids_and_leaf() {
        let mut session = SessionManager::in_memory();
        let first = session.append_message(user_msg("first")).unwrap();
        let second = session.append_message(assistant_msg("second")).unwrap();
        let third = session.append_thinking_level_change("high").unwrap();
        assert_eq!(session.get_entry(&first).unwrap().parent_id(), None);
        assert_eq!(
            session.get_entry(&second).unwrap().parent_id(),
            Some(first.as_str())
        );
        assert_eq!(
            session.get_entry(&third).unwrap().parent_id(),
            Some(second.as_str())
        );
        assert_eq!(session.position(), Some(third.as_str()));
        assert_eq!(session.get_entry_count(), 3);
    }

    #[test]
    fn custom_and_model_entries_integrate_into_tree() {
        let mut session = SessionManager::in_memory();
        let first = session.append_message(user_msg("hello")).unwrap();
        let custom = session
            .append_custom_entry("my_data", Some(json!({ "key": "value" })))
            .unwrap();
        let model = session.append_model_change("openai", "gpt-4").unwrap();
        match session.get_entry(&custom).unwrap() {
            SessionEntry::Custom {
                custom_type, data, ..
            } => {
                assert_eq!(custom_type, "my_data");
                assert_eq!(*data, Some(json!({ "key": "value" })));
            }
            other => panic!("unexpected entry: {}", other.entry_type()),
        }
        match session.get_entry(&model).unwrap() {
            SessionEntry::ModelChange {
                provider, model_id, ..
            } => {
                assert_eq!(provider, "openai");
                assert_eq!(model_id, "gpt-4");
            }
            other => panic!("unexpected entry: {}", other.entry_type()),
        }
        assert_eq!(
            session.get_entry(&model).unwrap().parent_id(),
            Some(custom.as_str())
        );
        assert_eq!(
            session.get_entry(&custom).unwrap().parent_id(),
            Some(first.as_str())
        );
    }

    #[test]
    fn get_branch_walks_to_root() {
        let mut session = SessionManager::in_memory();
        let first = session.append_message(user_msg("1")).unwrap();
        let second = session.append_message(assistant_msg("2")).unwrap();
        session.append_message(user_msg("3")).unwrap();
        assert_eq!(
            entry_ids(&session.get_branch(Some(&second))),
            vec![first.as_str(), second.as_str()]
        );
        assert_eq!(session.get_branch(None).len(), 3);
        assert!(SessionManager::in_memory().get_branch(None).is_empty());
    }

    #[test]
    fn get_tree_is_a_single_chain() {
        let mut session = SessionManager::in_memory();
        let first = session.append_message(user_msg("1")).unwrap();
        let second = session.append_message(assistant_msg("2")).unwrap();
        let tree = session.get_tree();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].entry.id(), first);
        assert_eq!(tree[0].children[0].entry.id(), second);
        assert!(tree[0].children[0].children.is_empty());
    }

    #[test]
    fn get_tree_contains_sibling_branches() {
        let mut session = SessionManager::in_memory();
        session.append_message(user_msg("1")).unwrap();
        let second = session.append_message(assistant_msg("2")).unwrap();
        let third = session.append_message(user_msg("3")).unwrap();
        session.branch(&second).unwrap();
        let fourth = session.append_message(user_msg("4")).unwrap();
        let node2 = &session.get_tree()[0].children[0];
        let child_ids: HashSet<&str> = node2
            .children
            .iter()
            .map(|child| child.entry.id())
            .collect();
        assert_eq!(child_ids, HashSet::from([third.as_str(), fourth.as_str()]));
    }

    #[test]
    fn get_tree_handles_multiple_branches_and_orphans() {
        let mut session = SessionManager::in_memory();
        session.append_message(user_msg("root")).unwrap();
        let second = session.append_message(assistant_msg("response")).unwrap();
        for text in ["a", "b", "c"] {
            session.branch(&second).unwrap();
            session.append_message(user_msg(text)).unwrap();
        }
        assert_eq!(session.get_tree()[0].children[0].children.len(), 3);

        let orphan = SessionEntry::Message {
            id: "orphan".to_owned(),
            parent_id: Some("missing".to_owned()),
            timestamp: "2025-01-01T00:00:00Z".to_owned(),
            message: user_msg("orphan"),
        };
        let mut raw = session.raw_entries.clone();
        raw.push(serde_json::to_value(orphan).unwrap());
        session.raw_entries = raw;
        session.rebuild_index();
        assert_eq!(session.get_tree().len(), 2);
    }

    #[test]
    fn branch_moves_leaf_and_rejects_unknown_ids() {
        let mut session = SessionManager::in_memory();
        let first = session.append_message(user_msg("1")).unwrap();
        session.append_message(assistant_msg("2")).unwrap();
        assert!(session.branch("nope").is_err());
        session.navigate_tree(&first).unwrap();
        assert_eq!(session.position(), Some(first.as_str()));
        let branched = session.append_message(user_msg("branched")).unwrap();
        assert_eq!(
            session.get_entry(&branched).unwrap().parent_id(),
            Some(first.as_str())
        );
        session.set_position(None).unwrap();
        assert_eq!(session.position(), None);
        let root = session.append_message(user_msg("new root")).unwrap();
        assert_eq!(session.get_entry(&root).unwrap().parent_id(), None);
    }

    #[test]
    fn reset_makes_next_append_a_root() {
        let mut session = SessionManager::in_memory();
        session.append_message(user_msg("1")).unwrap();
        session.reset();
        let root = session.append_message(user_msg("2")).unwrap();
        assert_eq!(session.get_entry(&root).unwrap().parent_id(), None);
    }

    #[test]
    fn branch_with_summary_records_source_and_destination() {
        let mut session = SessionManager::in_memory();
        let first = session.append_message(user_msg("1")).unwrap();
        session.append_message(assistant_msg("2")).unwrap();
        let third = session.append_message(user_msg("3")).unwrap();
        let summary = session
            .branch_with_summary(Some(&first), "abandoned", BranchSummaryExtra::default())
            .unwrap();
        assert_eq!(session.position(), Some(summary.as_str()));
        match session.get_entry(&summary).unwrap() {
            SessionEntry::BranchSummary {
                parent_id,
                from_id,
                summary: text,
                ..
            } => {
                assert_eq!(parent_id.as_deref(), Some(first.as_str()));
                assert_eq!(from_id, &third);
                assert_eq!(text, "abandoned");
            }
            other => panic!("unexpected entry: {}", other.entry_type()),
        }
        assert!(
            session
                .branch_with_summary(Some("missing"), "x", BranchSummaryExtra::default())
                .is_err()
        );
    }

    #[test]
    fn labels_set_clear_and_last_wins() {
        let mut session = SessionManager::in_memory();
        let id = session.append_message(user_msg("hello")).unwrap();
        assert_eq!(session.get_label(&id), None);
        session.append_label_change(&id, Some("first")).unwrap();
        let last = session.append_label_change(&id, Some("second")).unwrap();
        assert_eq!(session.get_label(&id), Some("second"));
        assert_eq!(
            session.get_label_timestamp(&id),
            Some(session.get_entry(&last).unwrap().timestamp())
        );
        let tree = session.get_tree();
        assert_eq!(tree[0].label.as_deref(), Some("second"));
        session.append_label_change(&id, None).unwrap();
        assert_eq!(session.get_label(&id), None);
        assert!(session.append_label_change("missing", Some("x")).is_err());
    }

    #[test]
    fn session_info_name_is_sanitized_and_readable() {
        let mut session = SessionManager::in_memory();
        session.append_session_info("  hello\nworld  ").unwrap();
        assert_eq!(session.get_session_name(), Some("hello world"));
        session.append_session_info("   ").unwrap();
        assert_eq!(session.get_session_name(), None);
    }

    #[test]
    fn context_from_linear_conversation() {
        let mut session = SessionManager::in_memory();
        session.append_message(user_msg("msg1")).unwrap();
        session.append_message(assistant_msg("msg2")).unwrap();
        session.append_thinking_level_change("high").unwrap();
        let context = session.build_session_context();
        assert_eq!(context.messages.len(), 2);
        assert_eq!(context.thinking_level, "high");
        assert_eq!(
            context.model,
            Some(ModelRef {
                provider: "anthropic".to_owned(),
                model_id: "test".to_owned()
            })
        );
    }

    #[test]
    fn context_follows_only_the_selected_branch() {
        let mut session = SessionManager::in_memory();
        session.append_message(user_msg("msg1")).unwrap();
        let second = session.append_message(assistant_msg("msg2")).unwrap();
        session.append_message(user_msg("abandoned")).unwrap();
        session.branch(&second).unwrap();
        session.append_message(assistant_msg("kept")).unwrap();
        let context = session.build_session_context();
        assert_eq!(context.messages.len(), 3);
        assert_eq!(context.messages[2]["content"][0]["text"], "kept");
    }

    #[test]
    fn compaction_context_keeps_summary_and_recent_entries() {
        let mut session = SessionManager::in_memory();
        let first = session.append_message(user_msg("first")).unwrap();
        session.append_message(assistant_msg("response")).unwrap();
        let kept = session.append_message(user_msg("second")).unwrap();
        session
            .append_compaction("summary so far", Some(&kept), 1000)
            .unwrap();
        session.append_message(assistant_msg("third")).unwrap();
        let context = session.build_session_context();
        assert_eq!(context.messages.len(), 3);
        assert_eq!(context.messages[0]["role"], "compactionSummary");
        assert_eq!(context.messages[0]["summary"], "summary so far");
        assert_eq!(
            context.messages[1]["content"],
            user_msg("second")["content"]
        );
        assert_eq!(context.messages[2]["content"][0]["text"], "third");
        assert_eq!(session.build_context_entries().len(), 3);
        let _ = first;
    }

    #[test]
    fn latest_compaction_wins_and_branch_summary_is_projected() {
        let mut session = SessionManager::in_memory();
        session.append_message(user_msg("start")).unwrap();
        session.append_compaction("old", None, 10).unwrap();
        let entry = session.append_message(user_msg("new")).unwrap();
        session
            .append_compaction("newest", Some(&entry), 20)
            .unwrap();
        assert_eq!(
            get_latest_compaction_entry(&session.entries)
                .unwrap()
                .entry_type(),
            "compaction"
        );
        match get_latest_compaction_entry(&session.entries).unwrap() {
            SessionEntry::Compaction { summary, .. } => assert_eq!(summary, "newest"),
            _ => unreachable!(),
        }
        session.branch(&entry).unwrap();
        let branch_id = session
            .branch_with_summary(Some(&entry), "tried", BranchSummaryExtra::default())
            .unwrap();
        session.append_message(user_msg("continued")).unwrap();
        let context = session.build_session_context();
        assert!(
            context
                .messages
                .iter()
                .any(|message| message["role"] == "branchSummary")
        );
        let _ = branch_id;
    }

    #[test]
    fn custom_message_and_context_edit_are_projected() {
        let mut session = SessionManager::in_memory();
        let custom = session
            .append_custom_message_entry("note", json!("hello"), false, Some(json!({ "x": 1 })))
            .unwrap();
        let context = session.build_session_context();
        assert_eq!(context.messages.len(), 1);
        assert_eq!(context.messages[0]["role"], "custom");
        assert_eq!(context.messages[0]["customType"], "note");
        let edit = session
            .append_context_edit(&custom, json!({ "content": "replaced" }))
            .unwrap();
        assert!(session.get_entry(&edit).is_some());
        assert_eq!(
            session.build_session_context().messages[0]["content"],
            "replaced"
        );
        session.append_context_edit(&custom, Value::Null).unwrap();
        assert!(session.build_session_context().messages.is_empty());
    }

    #[test]
    fn context_edit_rejects_invalid_targets() {
        let mut session = SessionManager::in_memory();
        let id = session.append_message(user_msg("hello")).unwrap();
        session.append_model_change("openai", "gpt-4").unwrap();
        let model_id = session.position().unwrap().to_owned();
        assert!(
            session
                .append_context_edit("missing", json!({ "content": "x" }))
                .is_err()
        );
        assert!(
            session
                .append_context_edit(&model_id, json!({ "content": "x" }))
                .is_err()
        );
        assert!(
            session
                .append_context_edit(&id, json!({ "bad": 1 }))
                .is_err()
        );
    }

    #[test]
    fn build_session_context_free_function_matches_manager() {
        let entries = vec![
            SessionEntry::Message {
                id: "1".to_owned(),
                parent_id: None,
                timestamp: "2025-01-01T00:00:00Z".to_owned(),
                message: user_msg("hello"),
            },
            SessionEntry::ThinkingLevelChange {
                id: "2".to_owned(),
                parent_id: Some("1".to_owned()),
                timestamp: "2025-01-01T00:00:01Z".to_owned(),
                thinking_level: "medium".to_owned(),
            },
        ];
        let context = build_session_context(&entries, None);
        assert_eq!(context.messages.len(), 1);
        assert_eq!(context.thinking_level, "medium");
        assert_eq!(context.model, None);
        assert!(build_context_entries(&entries, None).len() == 2);
    }

    #[test]
    fn parse_session_entries_skips_bad_lines_and_bom() {
        let content = "\u{feff}{}\nnot json\n{\"type\":\"session\",\"id\":\"x\"}\n\n";
        let entries = parse_session_entries(content);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1]["id"], "x");
    }

    #[test]
    fn load_entries_from_file_validates_and_repairs() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        assert!(load_entries_from_file(&path).unwrap().is_empty());
        write(&path, "not json\n");
        assert!(load_entries_from_file(&path).unwrap().is_empty());
        let content = "{\"type\":\"session\",\"id\":\"abc\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\nnot json\n{\"type\":\"message\",\"id\":\"1\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}";
        write(&path, content);
        assert_eq!(load_entries_from_file(&path).unwrap().len(), 2);
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{content}\n"));
    }

    #[test]
    fn open_rejects_non_session_content_without_modifying_it() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("bad.jsonl");
        let content = "{\"type\":\"message\",\"id\":\"1\"}\n";
        write(&path, content);
        let error = SessionManager::open(&path).unwrap_err();
        assert!(error.contains("not a valid pi session"));
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
    }

    #[test]
    fn open_initializes_empty_file_preserving_explicit_path() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("empty.jsonl");
        write(&path, "");
        let session = SessionManager::open(&path).unwrap();
        assert_eq!(session.get_session_file(), Some(path.as_path()));
        assert_eq!(lines(&path).len(), 1);
        let reopened = SessionManager::open(&path).unwrap();
        assert_eq!(reopened.get_session_id(), session.get_session_id());
    }

    #[test]
    fn v1_migration_adds_ids_and_converts_compaction_index() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("v1.jsonl");
        write(
            &path,
            "{\"type\":\"session\",\"id\":\"old\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n\
             {\"type\":\"message\",\"timestamp\":\"2025-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n\
             {\"type\":\"message\",\"timestamp\":\"2025-01-01T00:00:02Z\",\"message\":{\"role\":\"hookMessage\",\"content\":\"hook\",\"timestamp\":2}}\n\
             {\"type\":\"compaction\",\"timestamp\":\"2025-01-01T00:00:03Z\",\"summary\":\"s\",\"firstKeptEntryIndex\":1,\"tokensBefore\":10}\n",
        );
        let session = SessionManager::open(&path).unwrap();
        assert_eq!(session.get_header().version, Some(3));
        let ids: Vec<&str> = session.entries.iter().map(SessionEntry::id).collect();
        assert!(ids.iter().all(|id| id.len() == 8));
        assert_eq!(session.entries[0].parent_id(), None);
        assert_eq!(session.entries[1].parent_id(), Some(ids[0]));
        match &session.entries[1] {
            SessionEntry::Message { message, .. } => assert_eq!(message["role"], "custom"),
            _ => unreachable!(),
        }
        match &session.entries[2] {
            SessionEntry::Compaction {
                first_kept_entry_id,
                ..
            } => assert_eq!(first_kept_entry_id, ids[0]),
            _ => unreachable!(),
        }
        assert_eq!(lines(&path)[0]["version"], 3);
        assert_eq!(lines(&path)[3]["firstKeptEntryId"], ids[0]);
        assert!(lines(&path)[3].get("firstKeptEntryIndex").is_none());
    }

    #[test]
    fn v2_migration_only_renames_hook_messages() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("v2.jsonl");
        write(
            &path,
            "{\"type\":\"session\",\"version\":2,\"id\":\"v2\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n\
             {\"type\":\"message\",\"id\":\"abc12345\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:01Z\",\"message\":{\"role\":\"hookMessage\",\"content\":\"from hook\",\"timestamp\":1}}\n",
        );
        let session = SessionManager::open(&path).unwrap();
        assert_eq!(session.get_header().version, Some(3));
        assert_eq!(session.entries[0].id(), "abc12345");
        match &session.entries[0] {
            SessionEntry::Message { message, .. } => assert_eq!(message["role"], "custom"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn migration_is_idempotent() {
        let mut raw = vec![
            json!({ "type": "session", "version": 3, "id": "s", "timestamp": "2025-01-01T00:00:00Z", "cwd": "/tmp" }),
            json!({ "type": "message", "id": "abc12345", "parentId": null, "timestamp": "2025-01-01T00:00:01Z",
                    "message": { "role": "user", "content": "hi", "timestamp": 1 } }),
        ];
        assert!(!migrate_session_entries(&mut raw));
        assert_eq!(raw[1]["id"], "abc12345");
    }

    #[test]
    fn in_memory_entries_adopt_headerless_entries_verbatim() {
        let entries = vec![SessionEntry::Message {
            id: "abc12345".to_owned(),
            parent_id: None,
            timestamp: "2026-01-01T00:00:01Z".to_owned(),
            message: json!({ "role": "hookMessage", "content": "from a hook", "timestamp": 1 }),
        }];
        let session =
            SessionManager::in_memory_with_entries("/project", Some("restored"), entries.clone())
                .unwrap();
        assert_eq!(session.get_entries(), entries.as_slice());
        assert_eq!(session.get_session_id(), "restored");
        assert_eq!(
            session.get_header().cwd,
            resolve_path(Path::new("/project")).to_string_lossy()
        );
        assert_eq!(session.get_leaf_id(), Some("abc12345"));
    }

    #[test]
    fn create_defers_file_until_a_message() {
        let temp = TempDir::new().unwrap();
        let mut session =
            SessionManager::create_in(temp.path(), temp.path(), Some("created-session-id"))
                .unwrap();
        let path = session.get_session_file().unwrap().to_path_buf();
        assert!(path.to_string_lossy().contains("created-session-id"));
        assert!(!path.exists());
        session.append_model_change("anthropic", "claude").unwrap();
        session.append_thinking_level_change("off").unwrap();
        assert!(!path.exists());
        session.append_message(user_msg("question")).unwrap();
        assert!(path.exists());
        let stored = lines(&path);
        let kinds: Vec<&str> = stored
            .iter()
            .filter_map(|line| field_str(line, "type"))
            .collect();
        assert_eq!(
            kinds,
            vec![
                "session",
                "model_change",
                "thinking_level_change",
                "message"
            ]
        );
    }

    #[test]
    fn v3_round_trip_preserves_all_entry_kinds() {
        let temp = TempDir::new().unwrap();
        let mut session = SessionManager::create_in(temp.path(), temp.path(), None).unwrap();
        let first = session.append_message(user_msg("question")).unwrap();
        session.append_message(assistant_msg("answer")).unwrap();
        session
            .append_compaction_with_options(
                "summary",
                Some(&first),
                100,
                CompactionExtra {
                    details: Some(json!({ "version": 2 })),
                    usage: Some(json!({ "input": 1 })),
                    from_hook: Some(true),
                    ..CompactionExtra::default()
                },
            )
            .unwrap();
        session
            .append_usage(
                "cache_warm",
                "openai",
                "gpt-4",
                json!({ "input": 3 }),
                Some("note"),
            )
            .unwrap();
        session
            .append_custom_message_entry("ext", json!("hello"), true, None)
            .unwrap();
        session
            .append_label_change(&first, Some("checkpoint"))
            .unwrap();
        session.append_session_info("round trip").unwrap();
        let path = session.get_session_file().unwrap().to_path_buf();
        let reopened = SessionManager::open(&path).unwrap();
        assert_eq!(reopened.get_entries(), session.get_entries());
        assert_eq!(reopened.get_label(&first), Some("checkpoint"));
        assert_eq!(reopened.get_session_name(), Some("round trip"));
        assert_eq!(
            reopened.build_session_context().messages,
            session.build_session_context().messages
        );
    }

    #[test]
    fn append_failure_rolls_back_memory_and_file() {
        let temp = TempDir::new().unwrap();
        let mut session = SessionManager::create_in(temp.path(), temp.path(), None).unwrap();
        let first = session.append_message(user_msg("first")).unwrap();
        let path = session.get_session_file().unwrap().to_path_buf();
        let good_length = fs::metadata(&path).unwrap().len();
        let permissions = fs::metadata(&path).unwrap().permissions();
        let mut readonly = permissions.clone();
        readonly.set_readonly(true);
        fs::set_permissions(&path, readonly).unwrap();
        let failed = session.append_message(assistant_msg("lost"));
        fs::set_permissions(&path, permissions).unwrap();
        assert!(failed.is_err());
        assert_eq!(session.position(), Some(first.as_str()));
        assert_eq!(session.get_entries().len(), 1);
        assert_eq!(fs::metadata(&path).unwrap().len(), good_length);
        let second = session.append_message(assistant_msg("second")).unwrap();
        assert_eq!(
            session.get_entry(&second).unwrap().parent_id(),
            Some(first.as_str())
        );
        assert_eq!(SessionManager::open(&path).unwrap().get_entries().len(), 2);
    }

    #[test]
    fn fork_extracts_only_the_selected_path() {
        let mut session = SessionManager::in_memory();
        let first = session.append_message(user_msg("1")).unwrap();
        let second = session.append_message(assistant_msg("2")).unwrap();
        let third = session.append_message(user_msg("3")).unwrap();
        session.append_message(assistant_msg("4")).unwrap();
        session.branch(&third).unwrap();
        session.append_message(user_msg("5")).unwrap();
        session.append_label_change(&second, Some("keep")).unwrap();
        assert_eq!(session.fork(&second).unwrap(), None);
        let ids: Vec<&str> = session.entries.iter().map(SessionEntry::id).collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&first.as_str()));
        assert!(ids.contains(&second.as_str()));
        assert!(
            ids.iter()
                .any(|id| session.get_entry(id).unwrap().entry_type() == "label")
        );
        assert_eq!(session.get_label(&second), Some("keep"));
        assert!(session.fork("missing").is_err());
    }

    #[test]
    fn fork_rewires_children_of_dropped_labels() {
        let mut session = SessionManager::in_memory();
        let first = session.append_message(user_msg("hello")).unwrap();
        session
            .append_label_change(&first, Some("checkpoint"))
            .unwrap();
        let model = session
            .append_model_change("anthropic", "claude-test")
            .unwrap();
        let last = session.append_message(user_msg("followup")).unwrap();
        session.fork(&last).unwrap();
        assert_eq!(
            session.get_entry(&model).unwrap().parent_id(),
            Some(first.as_str())
        );
    }

    #[test]
    fn fork_from_copies_entries_and_sets_parent_session() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source.jsonl");
        write(
            &source,
            "{\"type\":\"session\",\"version\":3,\"id\":\"source\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n\
             {\"type\":\"message\",\"id\":\"entry-1\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n",
        );
        let sessions = temp.path().join("sessions");
        let forked = SessionManager::fork_from(
            &source,
            temp.path(),
            Some(sessions),
            Some("forked-session-id"),
        )
        .unwrap();
        assert_eq!(forked.get_header().id, "forked-session-id");
        assert_eq!(
            forked.get_header().parent_session.as_deref(),
            Some(source.to_string_lossy().as_ref())
        );
        assert_eq!(forked.get_entries().len(), 1);
        let path = forked.get_session_file().unwrap();
        assert!(path.to_string_lossy().contains("forked-session-id"));
        assert_eq!(lines(path).len(), 2);
    }

    #[test]
    fn list_sessions_sorts_by_activity_and_extracts_metadata() {
        let temp = TempDir::new().unwrap();
        let mut first = SessionManager::create_in(temp.path(), temp.path(), Some("first")).unwrap();
        first.append_message(user_msg("hello there")).unwrap();
        first.append_message(assistant_msg("hi")).unwrap();
        first.append_session_info("Named session").unwrap();
        let mut second =
            SessionManager::create_in(temp.path(), temp.path(), Some("second")).unwrap();
        second.append_message(user_msg("second question")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        second.append_message(assistant_msg("reply")).unwrap();
        let sessions = SessionManager::list_sessions(temp.path()).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "second");
        assert_eq!(sessions[1].id, "first");
        assert_eq!(sessions[1].name.as_deref(), Some("Named session"));
        assert_eq!(sessions[1].message_count, 2);
        assert_eq!(sessions[1].first_message, "hello there");
        assert_eq!(sessions[1].all_messages_text, "hello there hi");
        assert_eq!(sessions[0].first_message, "second question");
    }

    #[test]
    fn find_most_recent_filters_by_cwd() {
        let temp = TempDir::new().unwrap();
        let project_a = temp.path().join("project-a");
        let project_b = temp.path().join("project-b");
        fs::create_dir_all(&project_a).unwrap();
        fs::create_dir_all(&project_b).unwrap();
        let a = temp.path().join("a.jsonl");
        let b = temp.path().join("b.jsonl");
        write(
            &a,
            &format!(
                "{{\"type\":\"session\",\"id\":\"a\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"{}\"}}\n",
                project_a.display()
            ),
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        write(
            &b,
            &format!(
                "{{\"type\":\"session\",\"id\":\"b\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"{}\"}}\n",
                project_b.display()
            ),
        );
        assert_eq!(find_most_recent_session(temp.path(), None), Some(b.clone()));
        assert_eq!(
            find_most_recent_session(temp.path(), Some(&project_a)),
            Some(a.clone())
        );
        assert_eq!(SessionManager::find_by_id(temp.path(), "a"), Some(a));
        let invalid = temp.path().join("invalid.jsonl");
        write(&invalid, "{\"type\":\"other\"}\n");
        assert_eq!(
            find_most_recent_session(temp.path(), Some(Path::new("/nonexistent"))),
            None
        );
    }

    #[test]
    fn default_session_dir_encodes_cwd() {
        let dir = get_default_session_dir("/example/project");
        assert!(dir.ends_with("sessions/--example-project--"));
    }

    #[test]
    fn deep_tree_does_not_overflow() {
        let mut session = SessionManager::in_memory();
        let mut parent: Option<String> = None;
        for index in 0..5000 {
            let id = session
                .append_entry(SessionEntry::Custom {
                    id: String::new(),
                    parent_id: None,
                    timestamp: format!("2025-01-01T00:00:{:02}Z", index % 60),
                    custom_type: "bulk".to_owned(),
                    data: None,
                })
                .unwrap();
            parent = Some(id);
        }
        let tree = session.get_tree();
        assert_eq!(tree.len(), 1);
        assert_eq!(session.get_branch(parent.as_deref()).len(), 5000);
        let _ = tree;
    }
}

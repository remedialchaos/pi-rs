//! Crash log persistence (`~/.pi/agent/crashes.json`).
//!
//! Port of `packages/coding-agent/src/core/crash-log.ts`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{APP_NAME, VERSION, get_agent_dir};

pub const MAX_CRASH_RECORDS: usize = 5;
pub const MAX_CRASH_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashKind {
    UncaughtException,
    FatalError,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashRecord {
    pub timestamp: String,
    pub version: String,
    pub kind: CrashKind,
    pub message: String,
    pub stack: Option<String>,
    pub session_file: Option<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notified: Option<bool>,
}

pub fn crash_log_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("crashes.json")
}

pub fn default_crash_log_path() -> Result<PathBuf, String> {
    Ok(crash_log_path(&get_agent_dir()?))
}

pub fn read_crash_log_at(path: &Path) -> Vec<CrashRecord> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(&bytes) else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter_map(|item| {
            let timestamp = item.get("timestamp")?.as_str()?.to_owned();
            let message = item.get("message")?.as_str()?.to_owned();
            let version = item
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or(VERSION)
                .to_owned();
            let kind = item
                .get("kind")
                .and_then(|value| serde_json::from_value::<CrashKind>(value.clone()).ok())
                .unwrap_or(CrashKind::FatalError);
            let stack = item.get("stack").and_then(Value::as_str).map(str::to_owned);
            let session_file = item
                .get("sessionFile")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let cwd = item
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let notified = item.get("notified").and_then(Value::as_bool);
            Some(CrashRecord {
                timestamp,
                version,
                kind,
                message,
                stack,
                session_file,
                cwd,
                notified,
            })
        })
        .collect()
}

pub fn read_crash_log(agent_dir: &Path) -> Vec<CrashRecord> {
    read_crash_log_at(&crash_log_path(agent_dir))
}

fn write_crash_log(records: &[CrashRecord], path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut json = serde_json::to_string_pretty(records).map_err(|error| error.to_string())?;
    json.push('\n');
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, &json).map_err(|error| error.to_string())?;
    std::fs::rename(&tmp, path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        error.to_string()
    })
}

/// Best-effort persistence for callers that are already crashing.
pub fn record_crash_at(
    path: &Path,
    kind: CrashKind,
    message: impl Into<String>,
    stack: Option<String>,
    session_file: Option<String>,
    cwd: impl Into<String>,
) -> Option<CrashRecord> {
    let record = CrashRecord {
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        version: VERSION.to_owned(),
        kind,
        message: message.into(),
        stack,
        session_file,
        cwd: cwd.into(),
        notified: None,
    };
    let mut records = read_crash_log_at(path);
    records.push(record.clone());
    if records.len() > MAX_CRASH_RECORDS {
        let drain = records.len() - MAX_CRASH_RECORDS;
        records.drain(..drain);
    }
    write_crash_log(&records, path).ok()?;
    Some(record)
}

pub fn record_crash(
    agent_dir: &Path,
    kind: CrashKind,
    message: impl Into<String>,
    stack: Option<String>,
    session_file: Option<String>,
    cwd: impl Into<String>,
) -> Option<CrashRecord> {
    record_crash_at(
        &crash_log_path(agent_dir),
        kind,
        message,
        stack,
        session_file,
        cwd,
    )
}

fn current_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn parse_timestamp_ms(timestamp: &str) -> Option<u64> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .and_then(|dt| u64::try_from(dt.timestamp_millis()).ok())
}

/// Return the newest recent crash, marking pending records as announced.
pub fn take_unnotified_crash_at(path: &Path, now_ms: u64) -> Option<CrashRecord> {
    let records = read_crash_log_at(path);
    let crash = records
        .iter()
        .rev()
        .find(|record| {
            !record.notified.unwrap_or(false)
                && parse_timestamp_ms(&record.timestamp)
                    .is_some_and(|ts| now_ms.saturating_sub(ts) <= MAX_CRASH_AGE_MS)
        })
        .cloned()?;
    let updated = records
        .into_iter()
        .map(|mut record| {
            if record.notified != Some(true) {
                record.notified = Some(true);
            }
            record
        })
        .collect::<Vec<_>>();
    let _ = write_crash_log(&updated, path);
    Some(crash)
}

pub fn take_unnotified_crash(agent_dir: &Path) -> Option<CrashRecord> {
    take_unnotified_crash_at(&crash_log_path(agent_dir), current_epoch_ms())
}

pub fn clear_crash_log_at(path: &Path) {
    let _ = std::fs::remove_file(path);
}

pub fn clear_crash_log(agent_dir: &Path) {
    clear_crash_log_at(&crash_log_path(agent_dir));
}

pub fn format_crash_notice(crash: &CrashRecord) -> String {
    let when = DateTime::parse_from_rfc3339(&crash.timestamp)
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| crash.timestamp.clone());
    format!(
        "{APP_NAME} crashed on {when} ({}). Run /bug to report it; the crash details are attached automatically.",
        crash.message
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_caps_at_five_and_take_marks_notified_once() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = crash_log_path(dir.path());
        for idx in 1..=7 {
            record_crash(
                dir.path(),
                CrashKind::UncaughtException,
                format!("boom {idx}"),
                Some("trace".to_owned()),
                None,
                "/work",
            )
            .ok_or("expected record_crash to succeed")?;
        }
        let records = read_crash_log(dir.path());
        assert_eq!(records.len(), MAX_CRASH_RECORDS);
        assert_eq!(records[0].message, "boom 3");
        assert_eq!(records[4].message, "boom 7");

        let crash = take_unnotified_crash_at(&path, current_epoch_ms())
            .ok_or("expected unnotified crash")?;
        assert_eq!(crash.message, "boom 7");
        assert!(take_unnotified_crash_at(&path, current_epoch_ms()).is_none());

        clear_crash_log(dir.path());
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn expired_crash_is_not_returned_as_unnotified() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let path = crash_log_path(dir.path());
        let record = record_crash(
            dir.path(),
            CrashKind::FatalError,
            "old crash",
            None,
            None,
            "/work",
        )
        .ok_or("expected record_crash")?;
        let ts = parse_timestamp_ms(&record.timestamp).ok_or("invalid timestamp")?;
        assert!(take_unnotified_crash_at(&path, ts + MAX_CRASH_AGE_MS + 1).is_none());
        Ok(())
    }
}

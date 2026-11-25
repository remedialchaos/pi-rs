//! Timing instrumentation ported from `packages/coding-agent/src/core/timings.ts`.
//!
//! Upstream tracks startup phases in `main` and `extensions` namespaces behind
//! `PI_TIMING=1`. This module keeps that API and adds [`RequestTiming`] for
//! per-request measurements (model, provider, start/end, duration).

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use serde_json::{Map, Value};

/// Environment variable that enables timing output.
pub const PI_TIMING_ENV: &str = "PI_TIMING";

/// True when startup timing is enabled (`PI_TIMING=1`).
pub fn timing_enabled() -> bool {
    std::env::var(PI_TIMING_ENV).ok().as_deref() == Some("1")
}

/// Wall clock time in milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// One request timing, with optional model and provider labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestTiming {
    pub label: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub start_ms: u64,
    pub end_ms: Option<u64>,
}

impl RequestTiming {
    /// Start a timing using the current wall clock.
    pub fn new(label: impl Into<String>, provider: Option<&str>, model: Option<&str>) -> Self {
        Self::at(label, provider, model, now_ms())
    }

    /// Start a request timing with the default `"request"` label.
    pub fn start(provider: Option<&str>, model: Option<&str>) -> Self {
        Self::new("request", provider, model)
    }

    /// Start a timing with an explicit clock value, for deterministic callers.
    pub fn at(
        label: impl Into<String>,
        provider: Option<&str>,
        model: Option<&str>,
        start_ms: u64,
    ) -> Self {
        Self {
            label: label.into(),
            provider: provider.map(str::to_string),
            model: model.map(str::to_string),
            start_ms,
            end_ms: None,
        }
    }

    /// Mark the end of the timing at the current wall clock.
    pub fn finish(&mut self) -> Result<(), String> {
        self.finish_at(now_ms())
    }

    /// Mark the end of the timing at an explicit clock value.
    pub fn finish_at(&mut self, end_ms: u64) -> Result<(), String> {
        if self.end_ms.is_some() {
            return Err("Timing has already finished".to_string());
        }
        if end_ms < self.start_ms {
            return Err("Timing end precedes start".to_string());
        }
        self.end_ms = Some(end_ms);
        Ok(())
    }

    pub fn is_complete(&self) -> bool {
        self.end_ms.is_some()
    }

    pub fn duration_ms(&self) -> Option<u64> {
        self.end_ms.map(|end| end.saturating_sub(self.start_ms))
    }

    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("label".to_string(), Value::String(self.label.clone()));
        map.insert("startMs".to_string(), Value::from(self.start_ms));
        map.insert(
            "endMs".to_string(),
            self.end_ms.map_or(Value::Null, Value::from),
        );
        map.insert(
            "durationMs".to_string(),
            self.duration_ms().map_or(Value::Null, Value::from),
        );
        map.insert(
            "provider".to_string(),
            self.provider.clone().map_or(Value::Null, Value::String),
        );
        map.insert(
            "model".to_string(),
            self.model.clone().map_or(Value::Null, Value::String),
        );
        Value::Object(map)
    }
}

/// A printed timing entry. `provider` and `model` are present for request
/// timings and absent for startup phase timings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingEntry {
    pub label: String,
    pub ms: u64,
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl TimingEntry {
    fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("label".to_string(), Value::String(self.label.clone()));
        map.insert("ms".to_string(), Value::from(self.ms));
        if let Some(provider) = &self.provider {
            map.insert("provider".to_string(), Value::String(provider.clone()));
        }
        if let Some(model) = &self.model {
            map.insert("model".to_string(), Value::String(model.clone()));
        }
        Value::Object(map)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Namespace {
    timings: Vec<TimingEntry>,
    last_time_ms: u64,
}

impl Namespace {
    fn new(now_ms: u64) -> Self {
        Self {
            timings: Vec::new(),
            last_time_ms: now_ms,
        }
    }
}

/// Aggregate timing tracker, keyed by namespace (`"main"` upstream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timings {
    enabled: bool,
    namespaces: IndexMap<String, Namespace>,
}

impl Timings {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            namespaces: IndexMap::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Clear one namespace and restart its clock at `now_ms`.
    pub fn reset_at(&mut self, namespace: &str, now_ms: u64) {
        self.namespaces
            .insert(namespace.to_string(), Namespace::new(now_ms));
    }

    pub fn reset(&mut self, namespace: &str) {
        self.reset_at(namespace, now_ms());
    }

    pub fn reset_all(&mut self) {
        self.namespaces.clear();
    }

    /// Record the time since the previous event in `namespace`.
    pub fn time_at(&mut self, label: &str, namespace: &str, at_ms: u64) {
        if !self.enabled {
            return;
        }
        let entry = self
            .namespaces
            .entry(namespace.to_string())
            .or_insert_with(|| Namespace::new(at_ms));
        entry.timings.push(TimingEntry {
            label: label.to_string(),
            ms: at_ms.saturating_sub(entry.last_time_ms),
            provider: None,
            model: None,
        });
        entry.last_time_ms = at_ms;
    }

    pub fn time(&mut self, label: &str, namespace: &str) {
        self.time_at(label, namespace, now_ms());
    }

    /// Record a completed request timing. This does not advance the startup
    /// clock used by [`Timings::time`].
    pub fn record(&mut self, timing: RequestTiming, namespace: &str) {
        if !self.enabled {
            return;
        }
        let Some(ms) = timing.duration_ms() else {
            return;
        };
        let entry = self
            .namespaces
            .entry(namespace.to_string())
            .or_insert_with(|| Namespace::new(timing.start_ms));
        entry.timings.push(TimingEntry {
            label: timing.label,
            ms,
            provider: timing.provider,
            model: timing.model,
        });
    }

    /// Snapshot of one namespace's entries.
    pub fn get(&self, namespace: &str) -> Vec<TimingEntry> {
        self.namespaces
            .get(namespace)
            .map(|entry| entry.timings.clone())
            .unwrap_or_default()
    }

    /// Snapshot of all namespaces in insertion order.
    pub fn get_all(&self) -> IndexMap<String, Vec<TimingEntry>> {
        self.namespaces
            .iter()
            .map(|(name, entry)| (name.clone(), entry.timings.clone()))
            .collect()
    }

    pub fn to_json(&self) -> Value {
        let mut root = Map::new();
        for (name, namespace) in &self.namespaces {
            let entries = namespace
                .timings
                .iter()
                .map(TimingEntry::to_json)
                .collect::<Vec<_>>();
            let total = namespace
                .timings
                .iter()
                .fold(0_u64, |sum, entry| sum.saturating_add(entry.ms));
            root.insert(
                name.clone(),
                serde_json::json!({ "timings": entries, "totalMs": total }),
            );
        }
        Value::Object(root)
    }

    /// Print all namespaces to stderr, matching upstream's output format.
    pub fn print(&self) {
        if !self.enabled {
            return;
        }
        for (namespace, state) in &self.namespaces {
            if state.timings.is_empty() {
                continue;
            }
            eprintln!("\n--- Startup Timings: {namespace} ---");
            for entry in &state.timings {
                eprintln!("  {}: {}ms", entry.label, entry.ms);
            }
            let total = state
                .timings
                .iter()
                .fold(0_u64, |sum, entry| sum.saturating_add(entry.ms));
            eprintln!("  TOTAL: {total}ms");
            eprintln!("{}\n", "-".repeat(namespace.len() + 8));
        }
    }
}

static GLOBAL: OnceLock<Mutex<Timings>> = OnceLock::new();

fn global_timings() -> &'static Mutex<Timings> {
    GLOBAL.get_or_init(|| Mutex::new(Timings::new(timing_enabled())))
}

fn with_global<T>(callback: impl FnOnce(&mut Timings) -> T) -> T {
    let mut guard: MutexGuard<'static, Timings> = global_timings()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    callback(&mut guard)
}

pub fn reset_timings(namespace: &str) {
    if timing_enabled() {
        with_global(|timings| timings.reset(namespace));
    }
}

pub fn time(label: &str, namespace: &str) {
    if timing_enabled() {
        with_global(|timings| timings.time(label, namespace));
    }
}

pub fn record_request_timing(timing: RequestTiming, namespace: &str) {
    if timing_enabled() {
        with_global(|timings| timings.record(timing, namespace));
    }
}

pub fn get_timings(namespace: &str) -> Vec<TimingEntry> {
    with_global(|timings| timings.get(namespace))
}

pub fn print_timings() {
    if timing_enabled() {
        with_global(|timings| timings.print());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_timing_tracks_duration() -> Result<(), String> {
        let mut timing = RequestTiming::at("request", Some("openai"), Some("gpt-5"), 1000);
        assert!(!timing.is_complete());
        assert_eq!(timing.duration_ms(), None);
        timing.finish_at(1345)?;
        assert!(timing.is_complete());
        assert_eq!(timing.duration_ms(), Some(345));
        Ok(())
    }

    #[test]
    fn request_timing_rejects_double_finish_and_inverted_clock() {
        let mut timing = RequestTiming::at("request", None, None, 500);
        assert!(timing.finish_at(400).is_err());
        assert!(timing.finish_at(600).is_ok());
        assert!(timing.finish_at(700).is_err());
    }

    #[test]
    fn request_timing_serializes_provider_and_model() -> Result<(), String> {
        let mut timing = RequestTiming::start(Some("anthropic"), Some("claude"));
        timing.finish_at(timing.start_ms.saturating_add(12))?;
        let json = timing.to_json();
        assert_eq!(json["provider"], Value::String("anthropic".into()));
        assert_eq!(json["model"], Value::String("claude".into()));
        assert_eq!(json["durationMs"], Value::from(12));
        Ok(())
    }

    #[test]
    fn timings_accumulate_deltas_per_namespace() {
        let mut timings = Timings::new(true);
        timings.reset_at("main", 100);
        timings.time_at("boot", "main", 150);
        timings.time_at("plugins", "main", 210);
        let entries = timings.get("main");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].label, "boot");
        assert_eq!(entries[0].ms, 50);
        assert_eq!(entries[1].label, "plugins");
        assert_eq!(entries[1].ms, 60);
    }

    #[test]
    fn reset_clears_only_the_named_namespace() {
        let mut timings = Timings::new(true);
        timings.time_at("a", "main", 10);
        timings.time_at("b", "extensions", 20);
        timings.reset_at("main", 30);
        assert!(timings.get("main").is_empty());
        assert_eq!(timings.get("extensions").len(), 1);
        timings.reset_all();
        assert!(timings.get_all().is_empty());
    }

    #[test]
    fn disabled_timings_ignore_events() {
        let mut timings = Timings::new(false);
        timings.time_at("a", "main", 1);
        let mut request = RequestTiming::at("request", None, None, 0);
        let _ = request.finish_at(5);
        timings.record(request, "main");
        assert!(timings.get("main").is_empty());
    }

    #[test]
    fn record_keeps_request_metadata_and_total() -> Result<(), String> {
        let mut timings = Timings::new(true);
        let mut request = RequestTiming::at("request", Some("openai"), Some("gpt-5"), 100);
        request.finish_at(180)?;
        timings.record(request, "main");
        let entries = timings.get("main");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].ms, 80);
        assert_eq!(entries[0].provider.as_deref(), Some("openai"));
        assert_eq!(entries[0].model.as_deref(), Some("gpt-5"));
        assert_eq!(timings.to_json()["main"]["totalMs"], Value::from(80));
        Ok(())
    }

    #[test]
    fn incomplete_request_is_not_recorded() {
        let mut timings = Timings::new(true);
        timings.record(RequestTiming::at("request", None, None, 10), "main");
        assert!(timings.get("main").is_empty());
    }

    #[test]
    fn global_get_returns_empty_without_timing_events() {
        let entries = get_timings("pi-rs-timings-test-namespace");
        assert!(entries.is_empty());
    }
}

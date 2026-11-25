//! Conversations kept on the device (upstream `conversation-cache.ts` + `history.ts`).
//! The IndexedDB / `localStorage` I/O is a host concern; this module holds the portable pure logic:
//! the "newest entries without a gap" window, dropping live documents,
//! most-recently-used eviction, bounded JSON persistence, and live-delta reconciliation.

use crate::browser_plugins::{ProjectedMessage, ProjectedRole};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// The newest serialized size kept of one transcript (images make entries large).
pub const MAX_BYTES: usize = 4_000_000;
/// The most recently used conversations that stay on the device.
pub const MAX_CONVERSATIONS: usize = 20;
/// Serialized size above which the oldest retained entries are dropped (`history.ts`).
pub const PAGE_SIZE: usize = 100;

/// Schema version for persisted browser transcript caches.
pub const PERSISTENCE_SCHEMA_VERSION: u32 = 1;
/// `localStorage` key for persisted transcript snapshots.
pub const STORAGE_KEY_TRANSCRIPTS: &str = "p2-transcript-cache-v1";
/// Maximum serialized byte size for the entire persisted transcript store.
pub const MAX_PERSISTED_STORE_BYTES: usize = 2 * 1024 * 1024;
/// Maximum persisted rows retained per conversation.
pub const MAX_PERSISTED_ROWS_PER_CONVERSATION: usize = 200;

/// An entry of a transcript window, as plain data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: u64,
    /// Serialized size in bytes (upstream uses `JSON.stringify(entry).length`).
    pub size: usize,
}

/// An item of a transcript view: an entry or something that breaks the contiguous tail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptItem {
    Entry(Entry),
    Gap,
    Earlier,
}

/// A conversation's transcript view as the cache sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptView {
    pub conversation: String,
    pub items: Vec<TranscriptItem>,
    /// Documents carried with the view (`pi.live` and `pi.inbox` are dropped when kept).
    pub docs: IndexMap<String, String>,
}

/// A window of a conversation: `before` is the entry just before the kept tail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationView {
    pub conversation: String,
    pub docs: IndexMap<String, String>,
    pub entries: Vec<Entry>,
    pub before: Option<u64>,
}

/// What is persisted for one conversation (`Kept` upstream).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kept {
    pub key: String,
    pub saved_at: u64,
    pub view: ConversationView,
    pub sizes: Vec<(String, u64)>,
}

/// `keptWindow`: the newest entries of `view` without a gap among them, at most
/// `max_bytes` of them (at least one). `before` bounds what is known below them.
pub fn kept_window(view: &TranscriptView, max_bytes: usize) -> ConversationView {
    let items = &view.items;
    let mut start = items.len();
    let mut bytes = 0usize;
    while start > 0 {
        let Some(TranscriptItem::Entry(entry)) = items.get(start - 1) else {
            break;
        };
        // Always keep at least one entry, even when it alone exceeds the budget.
        if start < items.len() && bytes.saturating_add(entry.size) > max_bytes {
            break;
        }
        bytes = bytes.saturating_add(entry.size);
        start -= 1;
    }
    let entries: Vec<Entry> = items[start..]
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Entry(entry) => Some(entry.clone()),
            _ => None,
        })
        .collect();
    ConversationView {
        conversation: view.conversation.clone(),
        docs: view.docs.clone(),
        before: entries
            .first()
            .filter(|_| start > 0)
            .map(|first| first.id.saturating_sub(1)),
        entries,
    }
}

/// `keptView`: the transcript as kept, without a run's live state or inbox.
pub fn kept_view(view: &TranscriptView) -> ConversationView {
    let mut kept = kept_window(view, MAX_BYTES);
    kept.docs.shift_remove("pi.live");
    kept.docs.shift_remove("pi.inbox");
    kept
}

/// The cache keys of conversations that survive an eviction, most-recently-used first.
///
/// Upstream sorts all kept records by `savedAt` descending and deletes everything past
/// `MAX_CONVERSATIONS`. Returning the survivors keeps this logic pure and testable.
pub fn surviving_keys(kept: &[Kept], limit: usize) -> Vec<String> {
    let mut sorted: Vec<&Kept> = kept.iter().collect();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.saved_at));
    sorted
        .into_iter()
        .take(limit)
        .map(|entry| entry.key.clone())
        .collect()
}

/// Canonical cache key for a `(session_id, conversation)` pair.
pub fn conversation_cache_key(session_id: &str, conversation: u64) -> String {
    format!("{session_id}#{conversation}")
}

/// Serializable role for persisted transcript rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CachedRole {
    User,
    Assistant,
    Tool,
}

impl From<ProjectedRole> for CachedRole {
    fn from(role: ProjectedRole) -> Self {
        match role {
            ProjectedRole::User => Self::User,
            ProjectedRole::Assistant => Self::Assistant,
            ProjectedRole::Tool => Self::Tool,
        }
    }
}

impl From<CachedRole> for ProjectedRole {
    fn from(role: CachedRole) -> Self {
        match role {
            CachedRole::User => Self::User,
            CachedRole::Assistant => Self::Assistant,
            CachedRole::Tool => Self::Tool,
        }
    }
}

/// Serializable summary of an activity step for offline rendering.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedActivity {
    pub id: String,
    pub name: String,
    pub status: String,
    pub output: String,
    pub duration_ms: Option<u64>,
}

/// Persisted transcript row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedMessage {
    pub id: String,
    pub role: CachedRole,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activities: Vec<CachedActivity>,
}

impl CachedMessage {
    fn estimated_bytes(&self) -> usize {
        self.id.len()
            + self.text.len()
            + self
                .activities
                .iter()
                .map(|a| a.id.len() + a.name.len() + a.status.len() + a.output.len() + 32)
                .sum::<usize>()
            + 64
    }
}

/// Persisted snapshot of one conversation's settled transcript tail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedConversation {
    pub key: String,
    pub session_id: String,
    pub conversation: u64,
    pub saved_at: u64,
    pub messages: Vec<CachedMessage>,
}

/// Versioned, bounded offline transcript store serialized to `localStorage`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedTranscriptStore {
    pub version: u32,
    pub conversations: IndexMap<String, CachedConversation>,
}

impl Default for PersistedTranscriptStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PersistedTranscriptStore {
    pub fn new() -> Self {
        Self {
            version: PERSISTENCE_SCHEMA_VERSION,
            conversations: IndexMap::new(),
        }
    }

    /// Parses a persisted JSON payload, falling back to an empty store on corruption or
    /// incompatible schema version. Strips any `"live"` streaming rows and enforces row caps.
    pub fn from_json_or_empty(raw: Option<&str>) -> Self {
        let Some(raw) = raw else {
            return Self::new();
        };
        let Ok(mut store) = serde_json::from_str::<Self>(raw) else {
            return Self::new();
        };
        if store.version != PERSISTENCE_SCHEMA_VERSION {
            return Self::new();
        }
        for conv in store.conversations.values_mut() {
            conv.messages.retain(|msg| msg.id != "live");
            if conv.messages.len() > MAX_PERSISTED_ROWS_PER_CONVERSATION {
                let drop_n = conv.messages.len() - MAX_PERSISTED_ROWS_PER_CONVERSATION;
                conv.messages.drain(..drop_n);
            }
        }
        store.evict_lru(MAX_CONVERSATIONS);
        store
    }

    /// Returns cached messages for `(session_id, conversation)` if present.
    pub fn get(&self, session_id: &str, conversation: u64) -> Option<&CachedConversation> {
        let key = conversation_cache_key(session_id, conversation);
        self.conversations.get(&key)
    }

    /// Removes all cached conversations belonging to `session_id` (e.g., on guest revocation).
    pub fn remove_session(&mut self, session_id: &str) {
        self.conversations
            .retain(|_, conv| conv.session_id != session_id);
    }

    /// Inserts or replaces a conversation's settled messages, trimming to `max_bytes_per_conv`
    /// (always keeping at least the newest message when non-empty) and evicting LRU
    /// conversations beyond `max_conversations`.
    pub fn put(
        &mut self,
        session_id: &str,
        conversation: u64,
        messages: &[ProjectedMessage],
        saved_at: u64,
        max_conversations: usize,
        max_bytes_per_conv: usize,
    ) {
        let settled = cached_from_projected(messages);
        let bounded = trim_cached_tail(
            &settled,
            max_bytes_per_conv,
            MAX_PERSISTED_ROWS_PER_CONVERSATION,
        );
        let key = conversation_cache_key(session_id, conversation);
        self.conversations.insert(
            key.clone(),
            CachedConversation {
                key,
                session_id: session_id.to_owned(),
                conversation,
                saved_at,
                messages: bounded,
            },
        );
        self.evict_lru(max_conversations);
    }

    /// Evicts oldest conversations by `saved_at` until `self.conversations.len() <= limit`.
    pub fn evict_lru(&mut self, limit: usize) {
        if self.conversations.len() <= limit {
            return;
        }
        let mut order: Vec<(String, u64)> = self
            .conversations
            .iter()
            .map(|(k, v)| (k.clone(), v.saved_at))
            .collect();
        order.sort_by_key(|(_, saved_at)| std::cmp::Reverse(*saved_at));
        let keep: HashSet<String> = order.into_iter().take(limit).map(|(k, _)| k).collect();
        self.conversations.retain(|k, _| keep.contains(k));
    }

    /// Serializes the store into JSON guaranteed not to exceed `max_total_bytes`
    /// (evicting oldest conversations first, then trimming the remaining conversation's
    /// oldest rows if needed).
    pub fn to_bounded_json(&self, max_total_bytes: usize) -> String {
        let mut copy = self.clone();
        if let Ok(json) = serde_json::to_string(&copy)
            && json.len() <= max_total_bytes
        {
            return json;
        }
        while !copy.conversations.is_empty() {
            if let Ok(json) = serde_json::to_string(&copy)
                && json.len() <= max_total_bytes
            {
                return json;
            }
            if copy.conversations.len() > 1 {
                let oldest = copy
                    .conversations
                    .iter()
                    .min_by_key(|(_, v)| v.saved_at)
                    .map(|(k, _)| k.clone());
                if let Some(key) = oldest {
                    copy.conversations.shift_remove(&key);
                    continue;
                }
            }
            if let Some((_, only)) = copy.conversations.first_mut()
                && only.messages.len() > 1
            {
                only.messages.remove(0);
                continue;
            }
            copy.conversations.clear();
        }
        serde_json::to_string(&copy)
            .unwrap_or_else(|_| r#"{"version":1,"conversations":{}}"#.into())
    }
}

fn trim_cached_tail(
    messages: &[CachedMessage],
    max_bytes: usize,
    max_rows: usize,
) -> Vec<CachedMessage> {
    if messages.is_empty() {
        return Vec::new();
    }
    let mut start = messages.len();
    let mut bytes = 0usize;
    let mut rows = 0usize;
    while start > 0 {
        let msg = &messages[start - 1];
        let cost = msg.estimated_bytes();
        if rows > 0 && (rows >= max_rows || bytes.saturating_add(cost) > max_bytes) {
            break;
        }
        bytes = bytes.saturating_add(cost);
        rows += 1;
        start -= 1;
    }
    messages[start..].to_vec()
}

/// Converts projected transcript messages to settled `CachedMessage`s (dropping `"live"`/streaming).
pub fn cached_from_projected(messages: &[ProjectedMessage]) -> Vec<CachedMessage> {
    messages
        .iter()
        .filter(|msg| !msg.streaming && msg.id != "live")
        .map(|msg| CachedMessage {
            id: msg.id.clone(),
            role: msg.role.into(),
            text: msg.text.clone(),
            activities: msg
                .activities
                .iter()
                .map(|act| CachedActivity {
                    id: act.step.id.clone(),
                    name: act.step.name.clone(),
                    status: format!("{:?}", act.step.status),
                    output: act.step.output.clone(),
                    duration_ms: act.step.duration_ms,
                })
                .collect(),
        })
        .collect()
}

/// Converts cached messages into renderable `ProjectedMessage`s for immediate offline hydration.
pub fn projected_from_cached(cached: &[CachedMessage]) -> Vec<ProjectedMessage> {
    use crate::activity::detail::describe;
    use crate::activity::model::{Step, StepStatus, ToolActivityTable};
    use crate::browser_plugins::ChatActivity;

    let tools = ToolActivityTable::new();
    cached
        .iter()
        .map(|msg| {
            let activities = msg
                .activities
                .iter()
                .map(|act| {
                    let status = match act.status.as_str() {
                        "Done" => StepStatus::Done,
                        "Error" => StepStatus::Error,
                        _ => StepStatus::Running,
                    };
                    let step = Step {
                        id: act.id.clone(),
                        name: act.name.clone(),
                        status,
                        output: act.output.clone(),
                        duration_ms: act.duration_ms,
                    };
                    let described = describe(&step, &tools);
                    ChatActivity { step, described }
                })
                .collect();
            ProjectedMessage {
                id: msg.id.clone(),
                role: msg.role.into(),
                text: msg.text.clone(),
                streaming: false,
                activities,
            }
        })
        .collect()
}

/// Reconciles cached offline rows with an incoming live Chord `p2.chat` snapshot without
/// duplicates. When `preserve_earlier_prefix` is true (windowed live snapshot), cached
/// rows whose numeric ID precedes the earliest live numeric ID are preserved ahead of `live`.
pub fn reconcile_live_snapshot(
    cached: &[CachedMessage],
    live: &[ProjectedMessage],
    preserve_earlier_prefix: bool,
) -> Vec<ProjectedMessage> {
    if live.is_empty() {
        return projected_from_cached(cached);
    }
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    if preserve_earlier_prefix {
        let min_live_num = live.iter().filter_map(|m| m.id.parse::<u64>().ok()).min();
        if let Some(min_num) = min_live_num {
            let live_ids: HashSet<&str> = live.iter().map(|m| m.id.as_str()).collect();
            let prefix: Vec<CachedMessage> = cached
                .iter()
                .filter(|m| {
                    !live_ids.contains(m.id.as_str())
                        && m.id.parse::<u64>().is_ok_and(|id| id < min_num)
                })
                .cloned()
                .collect();
            for row in projected_from_cached(&prefix) {
                if seen.insert(row.id.clone()) {
                    out.push(row);
                }
            }
        }
    }

    for row in live {
        if seen.insert(row.id.clone()) {
            out.push(row.clone());
        } else if let Some(existing) = out.iter_mut().find(|m| m.id == row.id) {
            *existing = row.clone();
        }
    }

    out
}

/// Merges incremental live transcript rows onto `current` without duplicating IDs and
/// ensuring at most one trailing `"live"` streaming row.
pub fn merge_live_delta(
    current: &[ProjectedMessage],
    incoming: &[ProjectedMessage],
) -> Vec<ProjectedMessage> {
    let mut out: Vec<ProjectedMessage> = current
        .iter()
        .filter(|m| m.id != "live" && !m.streaming)
        .cloned()
        .collect();
    let mut index_by_id: HashMap<String, usize> = out
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.clone(), i))
        .collect();
    let mut live_row: Option<ProjectedMessage> = None;

    for row in incoming {
        if row.id == "live" || row.streaming {
            live_row = Some(row.clone());
            continue;
        }
        if let Some(&idx) = index_by_id.get(&row.id) {
            out[idx] = row.clone();
        } else {
            index_by_id.insert(row.id.clone(), out.len());
            out.push(row.clone());
        }
    }

    if let Some(live) = live_row {
        out.push(live);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(items: Vec<TranscriptItem>) -> TranscriptView {
        let mut docs = IndexMap::new();
        docs.insert("pi.live".into(), "running".into());
        docs.insert("pi.inbox".into(), "[]".into());
        docs.insert("title".into(), "Demo".into());
        TranscriptView {
            conversation: "c1".into(),
            items,
            docs,
        }
    }

    fn entry(id: u64, size: usize) -> TranscriptItem {
        TranscriptItem::Entry(Entry { id, size })
    }

    fn msg(id: &str, role: ProjectedRole, text: &str, streaming: bool) -> ProjectedMessage {
        ProjectedMessage {
            id: id.to_owned(),
            role,
            text: text.to_owned(),
            streaming,
            activities: Vec::new(),
        }
    }

    #[test]
    fn keeps_newest_contiguous_tail() {
        let v = view(vec![
            entry(1, 10),
            entry(2, 10),
            TranscriptItem::Gap,
            entry(3, 10),
            entry(4, 10),
        ]);
        let kept = kept_window(&v, 1000);
        assert_eq!(
            kept.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert_eq!(kept.before, Some(2));
    }

    #[test]
    fn drops_oldest_entries_over_byte_budget_but_keeps_one() {
        let v = view(vec![entry(1, 30), entry(2, 30), entry(3, 30)]);
        let kept = kept_window(&v, 50);
        // Two 30-byte entries (60) exceed the 50-byte budget, so only the newest stays.
        assert_eq!(
            kept.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![3]
        );
        let one = kept_window(&v, 1);
        assert_eq!(one.entries.len(), 1);
        assert_eq!(one.before, Some(2));
    }

    #[test]
    fn no_before_when_tail_starts_the_conversation() {
        let kept = kept_window(&view(vec![entry(1, 5), entry(2, 5)]), 1000);
        assert_eq!(kept.before, None);
    }

    #[test]
    fn kept_view_drops_live_documents() {
        let kept = kept_view(&view(vec![entry(1, 5)]));
        assert!(!kept.docs.contains_key("pi.live"));
        assert!(!kept.docs.contains_key("pi.inbox"));
        assert_eq!(kept.docs.get("title").map(String::as_str), Some("Demo"));
    }

    #[test]
    fn eviction_keeps_most_recently_used() {
        let kept = vec![
            Kept {
                key: "old".into(),
                saved_at: 1,
                view: kept_window(&view(vec![]), 1),
                sizes: vec![],
            },
            Kept {
                key: "new".into(),
                saved_at: 3,
                view: kept_window(&view(vec![]), 1),
                sizes: vec![],
            },
            Kept {
                key: "mid".into(),
                saved_at: 2,
                view: kept_window(&view(vec![]), 1),
                sizes: vec![],
            },
        ];
        assert_eq!(surviving_keys(&kept, 2), vec!["new", "mid"]);
    }

    #[test]
    fn persisted_store_round_trip_strips_live_and_handles_corruption() {
        let mut store = PersistedTranscriptStore::new();
        store.put(
            "s1",
            1,
            &[
                msg("1", ProjectedRole::User, "hello", false),
                msg("2", ProjectedRole::Assistant, "world", false),
                msg("live", ProjectedRole::Assistant, "typing...", true),
            ],
            100,
            20,
            1024 * 1024,
        );
        let json = store.to_bounded_json(MAX_PERSISTED_STORE_BYTES);
        let loaded = PersistedTranscriptStore::from_json_or_empty(Some(&json));
        let conv = loaded.get("s1", 1).expect("cached conversation");
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].text, "hello");
        assert_eq!(conv.messages[1].text, "world");

        assert!(
            PersistedTranscriptStore::from_json_or_empty(Some("{not valid json"))
                .conversations
                .is_empty()
        );
        assert!(
            PersistedTranscriptStore::from_json_or_empty(Some(
                r#"{"version":999,"conversations":{"s1#1":{"key":"s1#1","session_id":"s1","conversation":1,"saved_at":1,"messages":[]}}}"#
            ))
            .conversations
            .is_empty()
        );
        assert!(
            PersistedTranscriptStore::from_json_or_empty(None)
                .conversations
                .is_empty()
        );
    }

    #[test]
    fn persisted_store_evicts_lru_and_enforces_total_byte_cap() {
        let mut store = PersistedTranscriptStore::new();
        for i in 1..=4 {
            store.put(
                &format!("s{i}"),
                1,
                &[msg("1", ProjectedRole::User, &"x".repeat(120), false)],
                i as u64,
                3,
                10_000,
            );
        }
        assert_eq!(store.conversations.len(), 3);
        assert!(store.get("s1", 1).is_none());
        assert!(store.get("s4", 1).is_some());

        let bounded = store.to_bounded_json(500);
        assert!(bounded.len() <= 500);
        let parsed = PersistedTranscriptStore::from_json_or_empty(Some(&bounded));
        assert!(parsed.conversations.len() < 3);
    }

    #[test]
    fn live_delta_reconciliation_avoids_duplicates_and_preserves_prefix() {
        let cached = vec![
            CachedMessage {
                id: "1".into(),
                role: CachedRole::User,
                text: "first".into(),
                activities: vec![],
            },
            CachedMessage {
                id: "2".into(),
                role: CachedRole::Assistant,
                text: "old-answer".into(),
                activities: vec![],
            },
        ];

        // Full snapshot replaces stale cached row cleanly without duplicating IDs.
        let full_live = vec![
            msg("1", ProjectedRole::User, "first", false),
            msg("2", ProjectedRole::Assistant, "updated-answer", false),
            msg("3", ProjectedRole::Assistant, "follow-up", false),
        ];
        let reconciled = reconcile_live_snapshot(&cached, &full_live, false);
        assert_eq!(reconciled.len(), 3);
        assert_eq!(reconciled[1].text, "updated-answer");

        // Windowed live snapshot keeps earlier cached rows (< min live id).
        let windowed_live = vec![
            msg("2", ProjectedRole::Assistant, "updated-answer", false),
            msg("3", ProjectedRole::User, "next", false),
        ];
        let merged = reconcile_live_snapshot(&cached, &windowed_live, true);
        assert_eq!(
            merged.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["1", "2", "3"]
        );

        // Incremental delta replaces streaming "live" row with final settled row.
        let with_stream = vec![
            msg("1", ProjectedRole::User, "hi", false),
            msg("live", ProjectedRole::Assistant, "par...", true),
        ];
        let delta = vec![msg(
            "2",
            ProjectedRole::Assistant,
            "partial complete",
            false,
        )];
        let settled = merge_live_delta(&with_stream, &delta);
        assert_eq!(settled.len(), 2);
        assert_eq!(settled[1].id, "2");
        assert!(!settled[1].streaming);
    }
}

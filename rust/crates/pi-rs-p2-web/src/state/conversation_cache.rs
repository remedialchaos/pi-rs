//! Conversations kept on the device (upstream `conversation-cache.ts` + `history.ts`).
//! The IndexedDB I/O is a host concern; this module holds the portable pure logic:
//! the "newest entries without a gap" window, dropping live documents, and the
//! most-recently-used eviction of cached conversations.

use indexmap::IndexMap;

/// The newest serialized size kept of one transcript (images make entries large).
pub const MAX_BYTES: usize = 4_000_000;
/// The most recently used conversations that stay on the device.
pub const MAX_CONVERSATIONS: usize = 20;
/// Serialized size above which the oldest retained entries are dropped (`history.ts`).
pub const PAGE_SIZE: usize = 100;

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
}

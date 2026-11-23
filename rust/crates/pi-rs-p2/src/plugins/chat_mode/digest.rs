//! Pure digest and reply-quote logic for `chat-mode`.

use std::collections::HashSet;

use pi_rs_durable::{
    EntryRecord,
    harness::{InboxItem, InboxState},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::contract::{
    Attachment, AttachmentRef, Author, CHAT_ENTRY, ChatMessage, MENTION_ENTRY, Mention, ReplyTo,
    chat_message, is_valid_author_kind, is_valid_reply_to, mention,
};

/// At most this many chat messages go into one mention; older ones are counted, not quoted.
pub const MAX_DIGEST_MESSAGES: usize = 100;
/// At most this many of their attachments are listed (the newest).
pub const MAX_DIGEST_FILES: usize = 20;
/// Inline at most this many images per mention, the newest; the rest are listed by path.
pub const MAX_DIGEST_IMAGES: usize = 4;
/// At most this many characters of a replied-to message are quoted, whitespace collapsed.
pub const EXCERPT_LENGTH: usize = 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatEntry {
    pub id: u64,
    pub message: ChatMessage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitEntry {
    pub kind: String,
    pub data: Mention,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Submit {
    pub text: String,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when_busy: Option<String>,
    pub author: Author,
    pub files: Vec<Attachment>,
    pub max_images: usize,
    pub entry: SubmitEntry,
}

fn speaker(author: &Author, reply_to: Option<&ReplyTo>) -> String {
    let mut notes = Vec::new();
    if author.kind == "guest" {
        notes.push("guest".to_string());
    }
    if let Some(reply) = reply_to {
        notes.push(format!(
            "replying to {}: \"{}\"",
            reply.author.name, reply.excerpt
        ));
    }
    if notes.is_empty() {
        author.name.clone()
    } else {
        format!("{} ({})", author.name, notes.join(", "))
    }
}

/// The start of a message: whitespace collapsed, cut at `EXCERPT_LENGTH` UTF-16 code units with an ellipsis.
pub fn excerpt(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let utf16_len: usize = flat.chars().map(char::len_utf16).sum();
    if utf16_len <= EXCERPT_LENGTH {
        return flat;
    }
    let limit = EXCERPT_LENGTH.saturating_sub(1);
    let mut units = 0usize;
    let mut end_byte = 0usize;
    for (idx, ch) in flat.char_indices() {
        let ch_units = ch.len_utf16();
        if units + ch_units > limit {
            break;
        }
        units += ch_units;
        end_byte = idx + ch.len_utf8();
    }
    format!("{}…", flat[..end_byte].trim_end())
}

fn agent_author() -> Author {
    Author {
        kind: "agent".to_string(),
        id: "agent".to_string(),
        name: "Pi".to_string(),
    }
}

/// Extracts `MessageData` (`author`, `text`) from an entry's `data` if present and valid.
fn message_data(data: Option<&Value>) -> Option<(Author, String)> {
    let obj = data?.as_object()?;
    let author: Author = serde_json::from_value(obj.get("author")?.clone()).ok()?;
    if !is_valid_author_kind(&author.kind) {
        return None;
    }
    let text = obj.get("text")?.as_str()?.to_string();
    Some((author, text))
}

/// What a reply quotes from an entry, derived from the entry's own data.
pub fn quote_of(entry: &EntryRecord) -> Option<ReplyTo> {
    let make_quote = |author: Author, text: &str| -> Option<ReplyTo> {
        let result = ReplyTo {
            entry_id: entry.id,
            author,
            excerpt: excerpt(text),
        };
        is_valid_reply_to(&result).then_some(result)
    };

    if let Some(model) = &entry.model
        && let Some(first) = model.first()
        && first.get("role").and_then(Value::as_str) == Some("assistant")
    {
        let mut texts = Vec::new();
        if let Some(parts) = first.get("content").and_then(Value::as_array) {
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(t) = part.get("text").and_then(Value::as_str)
                {
                    texts.push(t);
                }
            }
        }
        return make_quote(agent_author(), &texts.join(" "));
    }

    let (author, text) = message_data(entry.data.as_ref())?;
    make_quote(author, &text)
}

fn boundary_of(entries: &[EntryRecord]) -> Option<u64> {
    for entry in entries {
        if entry.kind == MENTION_ENTRY
            && let Some(m) = mention(entry.data.as_ref())
            && let Some(&min_id) = m.included.iter().min()
        {
            return Some(min_id);
        }
    }
    None
}

/// Whether a newest-first read has gone far enough for `unmentioned` (the reader may stop).
pub fn read_enough(entries: &[EntryRecord]) -> bool {
    let Some(boundary) = boundary_of(entries) else {
        return false;
    };
    let last_id = entries.last().map_or(0, |e| e.id);
    last_id <= boundary
}

/// Chat messages no mention has passed on to Pi yet, oldest first.
/// `entries` are newest first; `inbox` holds mentions still queued.
pub fn unmentioned(entries: &[EntryRecord], inbox: Option<&InboxState>) -> Vec<ChatEntry> {
    let mut covered = HashSet::<u64>::new();
    if let Some(inbox) = inbox {
        for item in &inbox.items {
            let queued_entry = match item {
                InboxItem::Write { .. } => None,
                InboxItem::Steer { entry, .. } | InboxItem::FollowUp { entry, .. } => {
                    entry.as_ref()
                }
            };
            if let Some(e) = queued_entry
                && e.kind == MENTION_ENTRY
                && let Some(m) = mention(e.data.as_ref())
            {
                covered.extend(m.included);
            }
        }
    }

    let boundary = boundary_of(entries);
    let mut found = Vec::new();
    for entry in entries {
        let id = entry.id;
        if let Some(b) = boundary
            && id < b
        {
            break;
        }
        if entry.kind == MENTION_ENTRY
            && let Some(m) = mention(entry.data.as_ref())
        {
            covered.extend(m.included);
        }
        if entry.kind == CHAT_ENTRY
            && let Some(msg) = chat_message(entry.data.as_ref())
            && !covered.contains(&id)
        {
            found.push(ChatEntry { id, message: msg });
        }
    }
    found.reverse();
    found
}

/// The mention's text for the model: the chat not passed on yet, with its authors, then who asks and what.
pub fn digest(
    chat: &[ChatEntry],
    author: &Author,
    text: &str,
    answering: Option<&ReplyTo>,
) -> String {
    let start = chat.len().saturating_sub(MAX_DIGEST_MESSAGES);
    let quoted = &chat[start..];
    let omitted = chat.len() - quoted.len();
    let mut out = Vec::new();
    if !chat.is_empty() {
        out.push("<chat>".to_string());
        out.push(
            "Since you were last mentioned, people in this session talked to each other. They did not address you:"
                .to_string(),
        );
        if omitted > 0 {
            out.push(format!("({omitted} earlier messages omitted)"));
        }
        for entry in quoted {
            let files = if entry.message.attachments.is_empty() {
                String::new()
            } else {
                let names = entry
                    .message
                    .attachments
                    .iter()
                    .map(|f| f.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(" [attached: {names}]")
            };
            out.push(format!(
                "{}: {}{}",
                speaker(&entry.message.author, entry.message.reply_to.as_ref()),
                entry.message.text,
                files
            ));
        }
        out.push("</chat>".to_string());
        out.push(String::new());
    }
    out.push(format!("{} mentioned you:", speaker(author, answering)));
    out.push(text.to_string());
    out.join("\n")
}

/// Upload IDs to resolve for a mention: the chat's newest attachments, then the asker's own.
pub fn digest_files(chat: &[ChatEntry], own: &[String]) -> Vec<String> {
    let mut chat_ids: Vec<String> = chat
        .iter()
        .flat_map(|entry| entry.message.attachments.iter().map(|f| f.id.clone()))
        .collect();
    if chat_ids.len() > MAX_DIGEST_FILES {
        chat_ids = chat_ids.split_off(chat_ids.len() - MAX_DIGEST_FILES);
    }
    chat_ids.retain(|id| !own.contains(id));
    chat_ids.extend(own.iter().cloned());
    chat_ids
}

/// A mention as the chat plugin submits it.
pub struct ComposeMentionInput<'a> {
    pub author: Author,
    pub text: String,
    pub chat: &'a [ChatEntry],
    pub reply_to: Option<ReplyTo>,
    pub attachments: Vec<AttachmentRef>,
    pub files: Vec<Attachment>,
    pub request_id: String,
    pub when_busy: Option<String>,
}

pub fn compose_mention(input: ComposeMentionInput<'_>) -> Submit {
    let start = input.chat.len().saturating_sub(MAX_DIGEST_MESSAGES);
    let included = input.chat[start..].iter().map(|e| e.id).collect();
    let text = digest(
        input.chat,
        &input.author,
        &input.text,
        input.reply_to.as_ref(),
    );
    let data = Mention {
        author: input.author.clone(),
        text: input.text,
        attachments: input.attachments,
        reply_to: input.reply_to,
        included,
    };
    Submit {
        text,
        request_id: input.request_id,
        when_busy: input.when_busy,
        author: input.author,
        files: input.files,
        max_images: MAX_DIGEST_IMAGES,
        entry: SubmitEntry {
            kind: MENTION_ENTRY.to_string(),
            data,
        },
    }
}

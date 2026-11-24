//! Chat mode contracts, entry shapes, and mention matching helpers.

use std::sync::Arc;

use pi_rs_chord::{
    Result,
    contracts::{Contract, Member, schema as s},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::v1::p2_contracts::lookup;

/// Entry kind of a chat message: display-only, it carries no model message.
pub const CHAT_ENTRY: &str = "chat.message";
/// Entry kind of a mention: an input whose model message is the digest, and whose data is what the person wrote.
pub const MENTION_ENTRY: &str = "chat.mention";
/// Default name people call the agent by in chat mode.
pub const DEFAULT_AGENT_NAME: &str = "Pi";
/// Maximum attachments per message (`apps/p2/src/runtime/schemas.ts`).
pub const MAX_ATTACHMENTS: usize = 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Author {
    pub kind: String,
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentRef {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplyTo {
    pub entry_id: u64,
    pub author: Author,
    pub excerpt: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub author: Author,
    pub text: String,
    pub attachments: Vec<AttachmentRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<ReplyTo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mention {
    pub author: Author,
    pub text: String,
    pub attachments: Vec<AttachmentRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<ReplyTo>,
    pub included: Vec<u64>,
}

/// Validates `^[A-Za-z][A-Za-z0-9_-]{0,31}$`.
pub fn is_valid_agent_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    let rest_len = name.len() - 1;
    rest_len <= 31 && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Entries are history: read their data defensively, never assume this code wrote it.
pub fn chat_message(data: Option<&Value>) -> Option<ChatMessage> {
    let val = data?;
    let obj = val.as_object()?;
    let msg: ChatMessage = serde_json::from_value(Value::Object(obj.clone())).ok()?;
    if !is_valid_author_kind(&msg.author.kind) {
        return None;
    }
    if let Some(reply) = &msg.reply_to
        && !is_valid_reply_to(reply)
    {
        return None;
    }
    Some(msg)
}

/// Entries are history: read their data defensively, never assume this code wrote it.
pub fn mention(data: Option<&Value>) -> Option<Mention> {
    let val = data?;
    let obj = val.as_object()?;
    let m: Mention = serde_json::from_value(Value::Object(obj.clone())).ok()?;
    if !is_valid_author_kind(&m.author.kind) {
        return None;
    }
    if let Some(reply) = &m.reply_to
        && !is_valid_reply_to(reply)
    {
        return None;
    }
    Some(m)
}

pub(crate) fn is_valid_author_kind(kind: &str) -> bool {
    matches!(kind, "owner" | "guest" | "agent")
}

pub(crate) fn is_valid_reply_to(reply: &ReplyTo) -> bool {
    reply.entry_id >= 1 && is_valid_author_kind(&reply.author.kind)
}

/// The mention of the agent called `name`: `@` and the name in lower case ("@pi", "@claude").
pub fn mention_token(name: &str) -> String {
    format!("@{}", name.to_lowercase())
}

fn is_word_char(c: char) -> bool {
    c.is_alphabetic() || c.is_numeric() || c == '_'
}

fn is_mention_prev_blocked(c: char) -> bool {
    is_word_char(c) || c == '@'
}

fn is_mention_next_blocked(c: char) -> bool {
    is_word_char(c) || c == '-'
}

fn is_spoken_prev_blocked(c: char) -> bool {
    is_word_char(c) || matches!(c, '\'' | '’' | '@' | '-')
}

fn is_spoken_next_blocked(c: char) -> bool {
    is_word_char(c) || matches!(c, '\'' | '’' | '-')
}

/// Case-insensitive Unicode match of `needle` in `haystack` with custom boundary predicates.
fn matches_bounded(
    haystack: &str,
    needle: &str,
    prev_blocked: fn(char) -> bool,
    next_blocked: fn(char) -> bool,
) -> bool {
    if needle.is_empty() {
        return true;
    }
    let needle_lower = needle.to_lowercase();
    for (idx, _) in haystack.char_indices() {
        let rest = &haystack[idx..];
        if rest.len() < needle.len() && !needle.is_ascii() {
            // Still check char-by-char in case lowercase expands/shrinks.
        }
        // Fast path for ASCII needle against ASCII slice, fallback to lowercase prefix.
        let matched_len = if needle.is_ascii() {
            let bytes = rest.as_bytes();
            let nbytes = needle.as_bytes();
            if bytes.len() >= nbytes.len() && bytes[..nbytes.len()].eq_ignore_ascii_case(nbytes) {
                Some(nbytes.len())
            } else {
                None
            }
        } else {
            let mut n_chars = needle_lower.chars();
            let mut h_indices = rest.char_indices();
            let mut end = 0usize;
            let mut ok = true;
            for nc in &mut n_chars {
                let Some((h_off, hc)) = h_indices.next() else {
                    ok = false;
                    break;
                };
                if hc.to_lowercase().next() != Some(nc) {
                    ok = false;
                    break;
                }
                end = h_off + hc.len_utf8();
            }
            ok.then_some(end)
        };

        if let Some(len) = matched_len {
            let prev_ok = haystack[..idx]
                .chars()
                .next_back()
                .is_none_or(|c| !prev_blocked(c));
            let next_ok = rest[len..].chars().next().is_none_or(|c| !next_blocked(c));
            if prev_ok && next_ok {
                return true;
            }
        }
    }
    false
}

/// `@<name>` in any case, as a word of its own: with the default, "@pi", "@Pi," and "(@PI)" mention;
/// "@pizza" and "me@pi" do not.
pub fn mentions_agent(text: &str, name: &str) -> bool {
    let token = format!("@{name}");
    matches_bounded(
        text,
        &token,
        is_mention_prev_blocked,
        is_mention_next_blocked,
    )
}

/// A spoken message as chat mode reads it: one that names the agent gets its mention in front
/// (`"@pi can Pi summarize this"`). One already holding the mention stays as it is.
pub fn spoken_mention(text: &str, name: &str) -> String {
    if mentions_agent(text, name) {
        return text.to_string();
    }
    if matches_bounded(text, name, is_spoken_prev_blocked, is_spoken_next_blocked) {
        format!("{} {}", mention_token(name), text.trim())
    } else {
        text.to_string()
    }
}

/// `p2.chat-mode-setting` (local, v3).
pub fn chat_mode_setting_contract() -> Result<Arc<Contract>> {
    let agent_name = s::pattern(s::str(), "^[A-Za-z][A-Za-z0-9_-]{0,31}$");
    let contract = Contract::local(
        "p2.chat-mode-setting",
        3,
        vec![("conversation".into(), s::int())],
        indexmap::IndexMap::from([
            ("enabled".into(), Member::State(s::bool())),
            ("name".into(), Member::State(agent_name.clone())),
            ("waiting".into(), Member::State(s::num())),
            (
                "set".into(),
                Member::Method {
                    args: s::obj([("enabled", s::required(s::bool()))]),
                    result: s::nul(),
                },
            ),
            (
                "rename".into(),
                Member::Method {
                    args: s::obj([("name", s::required(agent_name))]),
                    result: s::nul(),
                },
            ),
        ]),
    )?;
    Ok(Arc::new(contract))
}

/// `p2.chat-mode` (remote, v2).
pub fn chat_mode_contract() -> Result<Arc<Contract>> {
    Ok(Arc::new(lookup("p2.chat-mode")?))
}

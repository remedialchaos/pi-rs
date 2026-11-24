//! Display helpers for `chat.message` and `chat.mention` transcript entries.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::contract::{CHAT_ENTRY, MENTION_ENTRY, ReplyTo, chat_message, mention};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplyDisplay {
    pub entry_id: u64,
    pub name: String,
    pub excerpt: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryDisplay {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<ReplyDisplay>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn quoted(reply_to: Option<ReplyTo>) -> Option<ReplyDisplay> {
    reply_to.map(|r| ReplyDisplay {
        entry_id: r.entry_id,
        name: r.author.name,
        excerpt: r.excerpt,
    })
}

fn plural(count: usize) -> String {
    if count == 1 {
        "1 chat message".to_string()
    } else {
        format!("{count} chat messages")
    }
}

pub fn chat_entry_display(kind: &str, data: Option<&Value>) -> Option<EntryDisplay> {
    match kind {
        CHAT_ENTRY => {
            let msg = chat_message(data)?;
            Some(EntryDisplay {
                reply_to: quoted(msg.reply_to),
                note: None,
            })
        }
        MENTION_ENTRY => {
            let asked = mention(data)?;
            let count = asked.included.len();
            Some(EntryDisplay {
                reply_to: quoted(asked.reply_to),
                note: (count > 0).then(|| format!("with {}", plural(count))),
            })
        }
        _ => None,
    }
}

//! Shared contracts and wire types for the `discord` plugin.

use indexmap::IndexMap;
use pi_rs_chord::{Result, contracts::Contract};
use serde::{Deserialize, Serialize};

use crate::{api::v1::p2_contracts::lookup, runtime::session_lock::SessionLock};

pub const DISCORD_BATCH: &str = "discord.batch";
pub const COMMANDS: [Command; 3] = [Command::Stop, Command::Status, Command::Compact];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Command {
    Stop,
    Status,
    Compact,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Wake {
    pub anyone: bool,
    pub users: Vec<String>,
    pub roles: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBinding {
    pub conversation: u64,
    pub name: String,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    pub bot_id: String,
    pub channel_id: String,
    pub channel_name: String,
    pub guild_id: String,
    pub guild_name: String,
    pub session: String,
    pub wake: Wake,
    pub cursor: Option<String>,
    pub threads: IndexMap<String, ThreadBinding>,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pending {
    pub bot_id: String,
    pub session: String,
    pub channel_id: String,
    pub reply_channel_id: String,
    pub message_id: String,
    pub submission_id: u64,
    pub since: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Guild {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum BotStatus {
    Connecting,
    Error {
        error: String,
    },
    Connected {
        user: String,
        #[serde(rename = "applicationId")]
        application_id: String,
        #[serde(rename = "avatarUrl", default, skip_serializing_if = "Option::is_none")]
        avatar_url: Option<String>,
        #[serde(rename = "inviteUrl")]
        invite_url: String,
        guilds: Vec<Guild>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotView {
    pub id: String,
    pub status: BotStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedId {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerDetails {
    pub channels: Vec<NamedId>,
    pub roles: Vec<NamedId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub session: String,
    pub bot: String,
    pub guild: String,
    pub channel: String,
    pub wake: Wake,
    pub sandbox: String,
    pub lock: SessionLock,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkUpdate {
    pub session: String,
    pub wake: Wake,
    pub lock: SessionLock,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferedFile {
    pub id: String,
    pub name: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscordFile {
    pub url: String,
    pub name: String,
    pub size: u64,
    pub key: String,
    pub ref_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordFileWire {
    pub url: String,
    pub name: String,
    pub size: u64,
    pub key: String,
    #[serde(rename = "ref")]
    pub ref_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum TurnState {
    Pending,
    Answered {
        text: String,
        files: Vec<OfferedFile>,
        answer: u64,
    },
    Stopped,
    Unanswered {
        reason: String,
        files: Vec<OfferedFile>,
    },
}

pub fn discord_store_contract() -> Result<Contract> {
    lookup("p2.discord-store")
}

pub fn discord_contract() -> Result<Contract> {
    lookup("p2.discord")
}

pub fn discord_ingress_contract() -> Result<Contract> {
    lookup("p2.discord-ingress")
}

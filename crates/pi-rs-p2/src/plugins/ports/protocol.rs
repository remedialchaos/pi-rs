//! Port-forward protocol: validation, Radius origins, and the view the owner and the agent see.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Opaque forward IDs: Radius puts them in a DNS label, so lowercase letters and digits only.
const ID_LENGTH: usize = 24;
/// The most characters a forward's name may have.
pub const MAX_FORWARD_NAME: usize = 40;

/// A stored forward (`forwards.json`), as the contract's `ForwardBase` knows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forward {
    pub id: String,
    /// A number from 1 to 65535.
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public: Option<bool>,
    /// The session whose agent opened it (the `ports` tool); absent when the owner added it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

impl Forward {
    /// The stored entry without its ID, as `forwards.json` keeps it.
    pub fn fields(&self) -> Value {
        let mut entry = Map::new();
        entry.insert("port".into(), json!(self.port));
        if let Some(name) = &self.name {
            entry.insert("name".into(), json!(name));
        }
        if self.public == Some(true) {
            entry.insert("public".into(), json!(true));
        }
        if let Some(session) = &self.session {
            entry.insert("session".into(), json!(session));
        }
        Value::Object(entry)
    }
}

/// Whether `id` is an opaque forward ID (`FORWARD_ID`).
pub fn is_forward_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= ID_LENGTH
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

/// A number from 1 to 65535, as `forwards.json` and the contract's methods take it.
pub fn validate_forward_port(value: &Value) -> Result<u16, PortError> {
    let port = value
        .as_u64()
        .filter(|port| (1..=65535).contains(port))
        .ok_or(PortError::Port)?;
    u16::try_from(port).map_err(|_| PortError::Port)
}

/// An optional forward name: at most 40 characters without control characters, trimmed empty to
/// absent.
pub fn validate_forward_name(value: Option<&Value>) -> Result<Option<String>, PortError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let name = value.as_str().ok_or(PortError::Name)?;
    if name.chars().count() > MAX_FORWARD_NAME
        || name.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f)
    {
        return Err(PortError::Name);
    }
    let trimmed = name.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_owned()))
}

static RADIUS_ORIGIN: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: the pattern is a compile-time constant that always compiles.
    Regex::new(r"^(https?://)([0-9a-f]{32})\.([^/]+)$").expect("compiled pattern")
});

/// The forward's origin beside the brain's Radius origin, or its public one; `None` when that
/// is not a Radius brain origin.
pub fn forward_origin(radius_origin: &str, id: &str, public: bool) -> Option<String> {
    let captures = RADIUS_ORIGIN.captures(radius_origin)?;
    if !is_forward_id(id) {
        return None;
    }
    let prefix = if public { "pub" } else { "fwd" };
    Some(format!(
        "{}{prefix}--{}--{}.{}",
        &captures[1], &captures[2], id, &captures[3]
    ))
}

/// A forward as shown to the owner and the agent: who opened it, and its addresses once known.
pub fn forward_view(
    forward: &Forward,
    radius_origin: Option<&str>,
    session_title: Option<&str>,
) -> Value {
    let mut fields = match serde_json::to_value(forward) {
        Ok(Value::Object(fields)) => fields,
        // INFALLIBLE: a plain struct serializes to a JSON object.
        _ => Map::new(),
    };
    let url = radius_origin.and_then(|origin| forward_origin(origin, &forward.id, false));
    let public_url = if let (Some(origin), Some(true)) = (radius_origin, forward.public) {
        forward_origin(origin, &forward.id, true)
    } else {
        None
    };
    if let Some(title) = session_title {
        fields.insert("sessionTitle".into(), json!(title));
    }
    if let Some(url) = url {
        fields.insert("url".into(), json!(format!("{url}/")));
    }
    if let Some(public_url) = public_url {
        fields.insert("publicUrl".into(), json!(format!("{public_url}/")));
    }
    Value::Object(fields)
}

/// `JSON.stringify` of a string, for the quoted labels the tool's description uses.
fn quoted(value: &str) -> String {
    match serde_json::to_string(value) {
        Ok(text) => text,
        // INFALLIBLE: serializing a string to JSON text cannot fail.
        Err(_) => format!("{value:?}"),
    }
}

/// The forward's line in the tool's and the Ports sheet's list.
fn label(forward: &Value) -> String {
    let name = forward["name"].as_str();
    format!(
        "port {}{}",
        forward["port"],
        name.map(|name| format!(" ({})", quoted(name)))
            .unwrap_or_default()
    )
}

/// What the agent learns from a `ports` reply: every forward, its addresses, who opened it, and
/// the public endpoints spelled out.
pub fn describe(reply: &Value, own_session: Option<&str>) -> String {
    let forwards = reply["forwards"].as_array().cloned().unwrap_or_default();
    let opener = |forward: &Value| match forward["session"].as_str() {
        Some(session) if Some(session) == own_session => "opened by this session".to_owned(),
        Some(session) => {
            let title = forward["sessionTitle"].as_str().unwrap_or(session);
            format!("opened by session {}", quoted(title))
        }
        None => "added by the owner".to_owned(),
    };
    let lines: Vec<String> = forwards
        .iter()
        .map(|forward| {
            format!(
                "- {}, {}, {}: {}{}",
                label(forward),
                if forward["public"].as_bool().unwrap_or(false) {
                    "PUBLIC"
                } else {
                    "private"
                },
                opener(forward),
                forward["url"].as_str().unwrap_or("address not known yet"),
                forward["publicUrl"]
                    .as_str()
                    .map(|url| format!(", public {url}"))
                    .unwrap_or_default(),
            )
        })
        .collect();
    let public_ones: Vec<&Value> = forwards
        .iter()
        .filter(|forward| forward["public"].as_bool().unwrap_or(false))
        .collect();
    [
        if lines.is_empty() {
            "No ports are forwarded.".to_owned()
        } else {
            "Forwarded ports:".to_owned()
        },
        lines.join("\n"),
        if public_ones.is_empty() {
            "No forward is public.".to_owned()
        } else {
            format!(
                "Public endpoints, open to anyone with the link without signing in: {}.",
                public_ones
                    .iter()
                    .map(|forward| {
                        format!(
                            "{} {}",
                            label(forward),
                            forward["publicUrl"]
                                .as_str()
                                .unwrap_or("(address not known yet)")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        },
    ]
    .into_iter()
    .chain(
        (!reply["addressesKnown"].as_bool().unwrap_or(false)).then(|| {
            "Addresses are not known yet: Pi has not been opened through Radius by its owner. \
             They appear once it has; the owner also finds them under Envs \u{2192} Ports."
                .to_owned()
        }),
    )
    .collect::<Vec<_>>()
    .join("\n")
}

/// Errors of the port-forward protocol, as the upstream messages state them.
#[derive(Debug, thiserror::Error)]
pub enum PortError {
    #[error("Port must be a number from 1 to 65535")]
    Port,
    #[error("Names have at most {MAX_FORWARD_NAME} characters")]
    Name,
    #[error("Invalid forwards.json")]
    InvalidStore,
    #[error("This is Pi's own port; it is already reachable through Radius")]
    Reserved,
    #[error("At most 50 forwards")]
    TooMany,
    #[error("Unknown forward")]
    Unknown,
    #[error("{0}")]
    Message(String),
}

impl From<PortError> for pi_rs_chord::Error {
    fn from(error: PortError) -> Self {
        Self::Invalid(error.to_string())
    }
}

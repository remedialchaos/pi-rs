//! The port forwards, kept in `forwards.json` of the data directory as
//! `{ "<id>": { "port", "name"?, "public"?, "session"? } }`.
//!
//! The owner manages them in the Ports sheet; a session's agent opens and closes them through
//! the `ports` tool (`session` names the session that opened one). The agent may change or
//! close any forward; a session's forwards close when it is archived.
//!
//! The server is the only reader and writer: the Radius relay client looks a forward up for
//! every relayed request, so the list lives in memory and every change is written through. A
//! forward's ID is random and becomes part of its URL; removing a forward and adding the same
//! port again gives a new URL, so a removed forward's links stop working.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use indexmap::IndexMap;
use pi_rs_chord::{Error, Result, context::Context, state::NativeSource};
use serde_json::{Map, Value};

use super::protocol::{
    Forward, PortError, is_forward_id, validate_forward_name, validate_forward_port,
};

/// The most forwards one server keeps.
const MAX_FORWARDS: usize = 50;
/// The alphabet of opaque forward IDs: lowercase letters and digits.
const ID_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
/// Random bytes per ID attempt, as upstream's `randomBytes(10)`.
const ID_BYTES: usize = 10;

/// `forwards.json` pretty-printed with tabs, as upstream's `JSON.stringify(value, null, "\t")`.
fn to_tabbed_json(value: &Value) -> Result<String> {
    use serde_json::ser::{PrettyFormatter, Serializer};
    let mut text = Vec::new();
    let formatter = PrettyFormatter::with_indent(b"\t");
    let mut serializer = Serializer::with_formatter(&mut text, formatter);
    serde::Serialize::serialize(value, &mut serializer)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    String::from_utf8(text).map_err(|e| Error::Invalid(e.to_string()))
}

fn lock(inner: &Mutex<Inner>) -> std::sync::MutexGuard<'_, Inner> {
    inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Inner {
    forwards: IndexMap<String, Forward>,
    radius_origin: Option<String>,
}

/// The open forwards store. Every mutation writes the whole list through to `forwards.json`
/// before it returns, and publishes a new revision the facet's state derives read.
pub struct Forwards {
    path: PathBuf,
    inner: Mutex<Inner>,
    /// Writes run one at a time, each with the whole list as it is then.
    saving: tokio::sync::Mutex<()>,
    /// Bumped after every published change; the state derives follow it.
    revision: NativeSource<u64>,
}

impl Forwards {
    /// Reads `forwards.json` (absent: empty) and validates every entry. `revision` is the
    /// publication the facet's state derives follow; the store bumps it after each change.
    pub async fn open(directory: &Path, revision: NativeSource<u64>) -> Result<Arc<Self>> {
        let path = directory.join("forwards.json");
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(Error::Invalid(error.to_string())),
        };
        let mut forwards = IndexMap::new();
        if let Some(text) = text {
            let value: Value = serde_json::from_str(&text).map_err(|_| PortError::InvalidStore)?;
            let entries = value.as_object().ok_or(PortError::InvalidStore)?;
            for (id, entry) in entries {
                let forward = parse_entry(id, entry)?;
                forwards.insert(id.clone(), forward);
            }
        }
        Ok(Arc::new(Self {
            path,
            inner: Mutex::new(Inner {
                forwards,
                radius_origin: None,
            }),
            saving: tokio::sync::Mutex::new(()),
            revision,
        }))
    }

    /// The forwards by port, then ID, as the list and the sheet show them.
    pub fn list(&self) -> Vec<Forward> {
        let inner = lock(&self.inner);
        let mut forwards: Vec<Forward> = inner.forwards.values().cloned().collect();
        forwards.sort_by(|a, b| a.port.cmp(&b.port).then_with(|| a.id.cmp(&b.id)));
        forwards
    }

    pub fn get(&self, id: &str) -> Option<Forward> {
        lock(&self.inner).forwards.get(id).cloned()
    }

    /// The brain's Radius origin: updated from relayed requests for its UI, kept in memory.
    pub fn radius_origin(&self) -> Option<String> {
        lock(&self.inner).radius_origin.clone()
    }

    /// Sets the Radius origin; `None` forgets it. Publishes when it changed.
    pub fn set_radius_origin(&self, origin: Option<&str>, context: &Context) -> Result<()> {
        let origin = origin
            .filter(|origin| !origin.is_empty())
            .map(str::to_owned);
        let changed = {
            let mut inner = lock(&self.inner);
            if inner.radius_origin == origin {
                false
            } else {
                inner.radius_origin = origin;
                true
            }
        };
        if changed {
            self.publish(context)?;
        }
        Ok(())
    }

    /// Tells the state derives a change landed.
    fn publish(&self, context: &Context) -> Result<()> {
        self.revision.set(self.revision.value() + 1, context)
    }

    /// `reserved`: ports that must never be forwarded, such as this server's own.
    pub async fn add(
        &self,
        port: u16,
        name: Option<&Value>,
        reserved: &[u16],
        public: bool,
        session: Option<&str>,
        context: &Context,
    ) -> Result<Forward> {
        if reserved.contains(&port) {
            return Err(PortError::Reserved.into());
        }
        let name = validate_forward_name(name)?;
        let forward = {
            let mut inner = lock(&self.inner);
            if inner.forwards.len() >= MAX_FORWARDS {
                return Err(PortError::TooMany.into());
            }
            let id = fresh_id(&inner.forwards);
            let forward = Forward {
                id,
                port,
                name,
                public: public.then_some(true),
                session: session
                    .filter(|session| !session.is_empty())
                    .map(str::to_owned),
            };
            inner.forwards.insert(forward.id.clone(), forward.clone());
            forward
        };
        self.write(context).await?;
        Ok(forward)
    }

    /// The `ports` tool's open: one forward per port. A port that is already forwarded keeps
    /// its address and its opener, and takes the new name and visibility, whoever opened it.
    pub async fn open_for_session(
        &self,
        session: &str,
        port: u16,
        name: Option<&Value>,
        public: Option<bool>,
        reserved: &[u16],
        context: &Context,
    ) -> Result<Forward> {
        let existing = lock(&self.inner)
            .forwards
            .values()
            .find(|forward| forward.port == port)
            .cloned();
        let Some(existing) = existing else {
            return self
                .add(
                    port,
                    name,
                    reserved,
                    public.unwrap_or(false),
                    Some(session),
                    context,
                )
                .await;
        };
        let name = match name {
            None => existing.name.clone(),
            Some(value) => validate_forward_name(Some(value))?,
        };
        let open = public.or(existing.public).unwrap_or(false);
        let updated = Forward {
            id: existing.id.clone(),
            port,
            name,
            public: open.then_some(true),
            session: existing.session.clone(),
        };
        lock(&self.inner)
            .forwards
            .insert(updated.id.clone(), updated.clone());
        self.write(context).await?;
        Ok(updated)
    }

    /// The `ports` tool's close, whoever opened the forward; `false` when the port was not
    /// forwarded.
    pub async fn close_port(&self, port: u16, context: &Context) -> Result<bool> {
        let existing = lock(&self.inner)
            .forwards
            .values()
            .find(|forward| forward.port == port)
            .map(|forward| forward.id.clone());
        let Some(id) = existing else {
            return Ok(false);
        };
        lock(&self.inner).forwards.shift_remove(&id);
        self.write(context).await?;
        Ok(true)
    }

    /// Closes every forward a session opened, as when it is archived.
    pub async fn close_session(&self, session: &str, context: &Context) -> Result<()> {
        let removed = {
            let mut inner = lock(&self.inner);
            let before = inner.forwards.len();
            inner
                .forwards
                .retain(|_, forward| forward.session.as_deref() != Some(session));
            before != inner.forwards.len()
        };
        if removed {
            self.write(context).await?;
        }
        Ok(())
    }

    /// Public forwards are open to anyone with the link; private ones only to the owner.
    pub async fn set_public(&self, id: &str, open: bool, context: &Context) -> Result<()> {
        {
            let mut inner = lock(&self.inner);
            let Some(forward) = inner.forwards.get_mut(id) else {
                return Err(PortError::Unknown.into());
            };
            forward.public = open.then_some(true);
        }
        self.write(context).await
    }

    pub async fn remove(&self, id: &str, context: &Context) -> Result<()> {
        if lock(&self.inner).forwards.shift_remove(id).is_none() {
            return Err(PortError::Unknown.into());
        }
        self.write(context).await
    }

    /// Writes the whole list to `forwards.json`, one write at a time, and publishes.
    async fn write(&self, context: &Context) -> Result<()> {
        let value = {
            let inner = lock(&self.inner);
            Value::Object(
                inner
                    .forwards
                    .iter()
                    .map(|(id, forward)| (id.clone(), forward.fields()))
                    .collect::<Map<String, Value>>(),
            )
        };
        let _serialized = self.saving.lock().await;
        let text = format!("{}\n", to_tabbed_json(&value)?);
        let tmp = self.tmp_path();
        write_private(&tmp, text.as_bytes()).await?;
        tokio::fs::rename(&tmp, &self.path)
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?;
        self.publish(context)
    }

    fn tmp_path(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(".tmp");
        self.path.with_file_name(name)
    }
}

/// One validated `forwards.json` entry.
fn parse_entry(id: &str, entry: &Value) -> Result<Forward> {
    if !is_forward_id(id) || !entry.is_object() {
        return Err(PortError::InvalidStore.into());
    }
    let port = validate_forward_port(entry.get("port").unwrap_or(&Value::Null))?;
    let name = validate_forward_name(entry.get("name"))?;
    if let Some(public) = entry.get("public")
        && !public.is_boolean()
    {
        return Err(PortError::InvalidStore.into());
    }
    let session = match entry.get("session") {
        None => None,
        Some(session) if session.as_str().is_some_and(|s| !s.is_empty()) => {
            session.as_str().map(str::to_owned)
        }
        Some(_) => return Err(PortError::InvalidStore.into()),
    };
    Ok(Forward {
        id: id.to_owned(),
        port,
        name,
        public: entry
            .get("public")
            .and_then(Value::as_bool)
            .filter(|open| *open),
        session,
    })
}

/// A fresh random ID that no forward uses.
fn fresh_id(forwards: &IndexMap<String, Forward>) -> String {
    loop {
        let bytes = uuid::Uuid::new_v4().into_bytes();
        let id: String = bytes[..ID_BYTES]
            .iter()
            .map(|byte| ID_ALPHABET[*byte as usize % ID_ALPHABET.len()] as char)
            .collect();
        if !forwards.contains_key(&id) {
            return id;
        }
    }
}

/// Writes `text` to `path`, private to this user where the platform knows file modes.
async fn write_private(path: &Path, text: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::io::AsyncWriteExt;
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .await;
        match file {
            Ok(mut file) => {
                file.write_all(text)
                    .await
                    .map_err(|e| Error::Invalid(e.to_string()))?;
                file.flush()
                    .await
                    .map_err(|e| Error::Invalid(e.to_string()))?;
            }
            Err(error) => return Err(Error::Invalid(error.to_string())),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::fs::write(path, text)
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?;
    }
    Ok(())
}

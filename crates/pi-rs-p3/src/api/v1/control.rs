//! The control messages between the server and a session's worker: JSON messages on the worker's
//! stdio, each line an envelope `{ "p3": message }`. The base's worker speaks them, the runner sends
//! them.
//!
//! Upstream puts them beside the link's frames on one Node IPC channel. `pi-rs-chord` does not yet
//! route a worker's links from the server (the `2.1b-3` gap), so this port's channel carries the
//! control messages alone; the envelope keeps the shape a link frame could share later.

use std::sync::Arc;

use pi_rs_chord::{
    Error as ChordError, Result as ChordResult,
    link::{Link, LinkEnd},
    node::workers::MAX_FRAME,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};

use crate::runner::PluginScript;

/// Multiplexed frame on the worker stdio IPC channel: control JSON (`0`) or binary Chord link (`1`).
#[derive(Clone, Debug)]
pub enum IpcFrame {
    Control(Vec<u8>),
    Binary(Vec<u8>),
}

/// Writes one bounded frame (`4-byte BE length || 1-byte tag || body`) to `writer`.
pub async fn write_ipc_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &IpcFrame,
) -> ChordResult<()> {
    let (tag, body) = match frame {
        IpcFrame::Control(bytes) => (0u8, bytes.as_slice()),
        IpcFrame::Binary(bytes) => (1u8, bytes.as_slice()),
    };
    if body.len() >= MAX_FRAME {
        return Err(ChordError::Invalid("worker IPC frame too large".into()));
    }
    writer.write_u32((body.len() + 1) as u32).await?;
    writer.write_u8(tag).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one bounded frame from `reader`.
pub async fn read_ipc_frame<R: AsyncRead + Unpin>(reader: &mut R) -> ChordResult<IpcFrame> {
    let length = reader.read_u32().await? as usize;
    if !(1..=MAX_FRAME).contains(&length) {
        return Err(ChordError::Invalid(
            "invalid worker IPC frame length".into(),
        ));
    }
    let tag = reader.read_u8().await?;
    let mut body = vec![0; length - 1];
    reader.read_exact(&mut body).await?;
    match tag {
        0 => Ok(IpcFrame::Control(body)),
        1 => Ok(IpcFrame::Binary(body)),
        _ => Err(ChordError::Invalid("invalid worker IPC tag".into())),
    }
}

/// Chord `Link` multiplexed over the worker stdio IPC channel.
pub struct IpcPipeLink {
    end: LinkEnd,
    outgoing: mpsc::Sender<IpcFrame>,
}

impl IpcPipeLink {
    pub fn new(outgoing: mpsc::Sender<IpcFrame>) -> Arc<Self> {
        Arc::new(Self {
            end: LinkEnd::default(),
            outgoing,
        })
    }

    /// Delivers an inbound binary Chord frame from the IPC reader.
    pub fn deliver(&self, bytes: Vec<u8>) {
        self.end.deliver(bytes);
    }

    /// Marks the link ended when the IPC reader/writer terminates.
    pub fn ended(&self) {
        self.end.ended();
    }
}

impl Link for IpcPipeLink {
    fn send(&self, bytes: Vec<u8>) -> ChordResult<()> {
        if self.closed() {
            return Err(ChordError::Closed);
        }
        if bytes.len() >= MAX_FRAME || self.outgoing.try_send(IpcFrame::Binary(bytes)).is_err() {
            self.close();
            return Err(ChordError::Invalid("worker IPC backpressure".into()));
        }
        Ok(())
    }

    fn close(&self) {
        self.end.ended();
    }

    fn on_message(&self, listener: Arc<dyn Fn(Vec<u8>) + Send + Sync>) {
        self.end.on_message(listener);
    }

    fn on_close(&self, listener: Arc<dyn Fn() + Send + Sync>) {
        self.end.on_close(listener);
    }

    fn closed(&self) -> bool {
        self.end.closed()
    }
}

/// What the server sends a worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToWorker {
    Start {
        session: String,
        /// The worker's private directory: its store lives in it.
        dir: String,
        /// How long it idles before it stops itself.
        idle_ms: u64,
        scripts: Vec<PluginScript>,
    },
    Reload {
        scripts: Vec<PluginScript>,
    },
    Shutdown,
}

/// What a worker sends the server: `failed` names the plugins that did not load; `closed`: it was
/// idle for its idle time and exits now (a clean exit, no crash).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FromWorker {
    Ready { failed: Vec<String> },
    Reloaded { failed: Vec<String> },
    ReloadFailed { error: String },
    Closed { reason: CloseReason },
}

/// Why a worker closed itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    Idle,
}

/// A control message in its envelope, as it goes over the channel.
pub fn worker_message<M: Serialize>(message: &M) -> Value {
    let mut envelope = Map::new();
    match serde_json::to_value(message) {
        Ok(value) => {
            envelope.insert("p3".into(), value);
        }
        Err(_) => {
            // INFALLIBLE: ToWorker and FromWorker are plain data with no map keys and no non-string
            // values; serializing one cannot fail. An impossible failure becomes an empty envelope,
            // which every reader rejects as no control message.
        }
    }
    Value::Object(envelope)
}

/// The control message in an envelope, or `None` for a line that is no message of this kind.
pub fn worker_control<M: DeserializeOwned>(message: &Value) -> Option<M> {
    serde_json::from_value(message.get("p3")?.clone()).ok()
}

/// One frame's bytes: the envelope, then a newline.
pub fn encode_line<M: Serialize>(message: &M) -> Vec<u8> {
    let mut line = serde_json::to_vec(&worker_message(message)).unwrap_or_default();
    line.push(b'\n');
    line
}

/// The message a frame's line holds.
pub fn decode_line<M: DeserializeOwned>(line: &[u8]) -> Option<M> {
    let value: Value = serde_json::from_slice(line).ok()?;
    worker_control(&value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_start_message_round_trips_in_its_envelope() {
        let message = ToWorker::Start {
            session: "s1".into(),
            dir: "/tmp/w".into(),
            idle_ms: 300,
            scripts: vec!["a.chord".into()],
        };
        let line = encode_line(&message);
        assert_eq!(line.last(), Some(&b'\n'));
        let value: Value = serde_json::from_slice(&line[..line.len() - 1]).expect("json");
        assert_eq!(value["p3"]["kind"], json!("start"));
        assert_eq!(decode_line::<ToWorker>(&line), Some(message));
    }

    #[test]
    fn every_worker_message_has_its_upstream_tag() {
        let cases = [
            (FromWorker::Ready { failed: vec![] }, json!("ready")),
            (FromWorker::Reloaded { failed: vec![] }, json!("reloaded")),
            (
                FromWorker::ReloadFailed { error: "no".into() },
                json!("reload_failed"),
            ),
            (
                FromWorker::Closed {
                    reason: CloseReason::Idle,
                },
                json!("closed"),
            ),
        ];
        for (message, tag) in cases {
            let value = worker_message(&message);
            assert_eq!(value["p3"]["kind"], tag);
            assert_eq!(worker_control::<FromWorker>(&value), Some(message));
        }
        assert_eq!(
            worker_message(&ToWorker::Shutdown),
            json!({ "p3": { "kind": "shutdown" } })
        );
    }

    #[test]
    fn a_line_that_is_no_control_message_is_ignored() {
        for line in [
            &b""[..],
            b"\n",
            b"{\"other\":1}\n",
            b"{\"p3\":{\"kind\":\"bogus\"}}\n",
            b"not json\n",
        ] {
            assert_eq!(decode_line::<FromWorker>(line), None, "{line:?}");
        }
    }
}

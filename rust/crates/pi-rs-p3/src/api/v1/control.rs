//! The control messages between the server and a session's worker: JSON messages on the worker's
//! stdio, each line an envelope `{ "p3": message }`. The base's worker speaks them, the runner sends
//! them.
//!
//! Upstream puts them beside the link's frames on one Node IPC channel. `pi-rs-chord` does not yet
//! route a worker's links from the server (the `2.1b-3` gap), so this port's channel carries the
//! control messages alone; the envelope keeps the shape a link frame could share later.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};

use crate::runner::PluginScript;

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

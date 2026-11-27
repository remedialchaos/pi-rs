//! Loopback provider fixture for the two documentation provider scenarios.
use super::*;
use serde_json::json;
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

pub(super) struct Server {
    pub origin: String,
    pub stream: bool,
    seen: Arc<AtomicBool>,
    task: JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    pub fn start(stream: bool) -> io::Result<Self> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let origin = format!("http://{}", listener.local_addr()?);
        let listener = TcpListener::from_std(listener)?;
        let seen = Arc::new(AtomicBool::new(false));
        let receiver = seen.clone();
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let _ =
                    tokio::time::timeout(Duration::from_secs(5), serve(socket, stream, &receiver))
                        .await;
            }
        });
        Ok(Self {
            origin,
            stream,
            seen,
            task,
        })
    }
    pub fn reset(&self) {
        self.seen.store(false, Ordering::Release);
    }
    pub fn valid(&self) -> bool {
        self.seen.load(Ordering::Acquire)
    }
    pub fn provider(&self) -> &'static str {
        if self.stream { "acme-stream" } else { "acme" }
    }
    pub fn model(&self) -> &'static str {
        if self.stream {
            "acme-stream-chat"
        } else {
            "acme-chat"
        }
    }
}
pub(super) fn api_documentation() -> String {
    json!({"name":"Acme Streaming API","request":{"method":"POST","path":"/generate","headers":{"content-type":"application/json","x-acme-key":"resolved credential"},"body":{"model":"acme-stream-chat","messages":[{"role":"user","content":"Hello"}],"stream":true}},"response":{"contentType":"application/x-ndjson","events":[{"type":"text_delta","text":"Hello"},{"type":"usage","input_tokens":3,"output_tokens":2},{"type":"done","reason":"stop"}]}}).to_string()+"\n"
}

async fn serve(socket: TcpStream, stream: bool, seen: &AtomicBool) -> io::Result<()> {
    let mut reader = BufReader::new(socket);
    let mut header = Vec::new();
    let mut bounded = (&mut reader).take(32 * 1024);
    loop {
        let start = header.len();
        if bounded.read_until(b'\n', &mut header).await? == 0 {
            return Ok(());
        }
        if &header[start..] == b"\r\n" {
            break;
        }
    }
    let mut lines = std::str::from_utf8(&header)
        .map_err(io::Error::other)?
        .lines();
    let request = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>();
    let headers: BTreeMap<_, _> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim()))
        .collect();
    let body_len = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if body_len > 1024 * 1024 {
        return Ok(());
    }
    let mut body = vec![0; body_len];
    reader.read_exact(&mut body).await?;
    let payload = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    let path = if stream {
        "/generate"
    } else {
        "/v1/chat/completions"
    };
    let (status, mime, body) = if request != ["POST", path] {
        ("404 Not Found", "text/plain", "Unknown endpoint".to_owned())
    } else if (stream && headers.get("x-acme-key") != Some(&"resolved-stream-key"))
        || (!stream && headers.get("authorization") != Some(&"Bearer resolved-acme-key"))
    {
        (
            "401 Unauthorized",
            "text/plain",
            "Invalid Acme credential".to_owned(),
        )
    } else {
        let messages = payload["messages"].as_array();
        let prompt = messages
            .and_then(|v| v.iter().find(|m| m["role"] == "user"))
            .and_then(|m| m["content"].as_str());
        let model = if stream {
            "acme-stream-chat"
        } else {
            "acme-chat"
        };
        if payload["model"] != model || payload["stream"] != true || prompt.is_none() {
            (
                "422 Unprocessable Entity",
                "text/plain",
                "Invalid Acme request".to_owned(),
            )
        } else {
            seen.store(
                prompt
                    == Some(if stream {
                        "Reply with ACME_STREAM_OK."
                    } else {
                        "Reply with ACME_OK."
                    }),
                Ordering::Release,
            );
            if stream {
                (
                    "200 OK",
                    "application/x-ndjson",
                    [
                        json!({"type":"text_delta","text":"ACME_"}),
                        json!({"type":"text_delta","text":"STREAM_OK"}),
                        json!({"type":"usage","input_tokens":4,"output_tokens":3}),
                        json!({"type":"done","reason":"stop"}),
                    ]
                    .into_iter()
                    .map(|v| v.to_string() + "\n")
                    .collect(),
                )
            } else {
                let mut first = json!({"id":"chatcmpl-acme","object":"chat.completion.chunk","created":0,"model":model,"choices":[{"index":0,"delta":{"role":"assistant","content":"ACME_OK"},"finish_reason":null}]});
                let text = first.to_string();
                first["choices"] = json!([{"index":0,"delta":{},"finish_reason":"stop"}]);
                first["usage"] = json!({"prompt_tokens":3,"completion_tokens":2});
                (
                    "200 OK",
                    "text/event-stream",
                    format!("data: {text}\n\ndata: {first}\n\ndata: [DONE]\n\n"),
                )
            }
        }
    };
    reader.into_inner().write_all(format!("HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await
}
#[cfg(test)]
mod tests;

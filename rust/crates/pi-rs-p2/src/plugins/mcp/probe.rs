//! The loopback probe: a listener on the p2 host's 127.0.0.1 that tells a sign-in page whether its
//! browser runs on the p2 host.
//!
//! The page (on p2's Radius or public origin) fetches `http://127.0.0.1:<port>/<nonce>`: only a
//! browser on this machine reaches this listener, and only this p2 knows the nonce, so another
//! program or p2 on the browser's machine cannot answer for it. A wrong answer costs nothing more
//! than a sign-in to paste.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use uuid::Uuid;

/// How long a nonce answers.
const NONCE_LIFETIME: Duration = Duration::from_secs(5 * 60);

/// A nonce and the page it was handed to.
struct Nonce {
    origin: String,
    expires: Instant,
}

/// The nonces and the listener of one probe.
#[derive(Default)]
struct Inner {
    nonces: Arc<Mutex<HashMap<String, Nonce>>>,
    listener: tokio::sync::Mutex<Option<Arc<TcpListener>>>,
}

/// The probe listener, started on first use.
#[derive(Clone, Default)]
pub struct LoopbackProbe {
    inner: Arc<Inner>,
}

impl LoopbackProbe {
    /// A probe with no listener yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A probe URL for a page on `origin`: it answers that origin's requests, with CORS, for a few minutes.
    pub async fn url(&self, origin: &str) -> Result<String, String> {
        let now = Instant::now();
        let nonce = {
            let mut nonces = self
                .inner
                .nonces
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            nonces.retain(|_, entry| entry.expires > now);
            let nonce = hex(&random_bytes(24));
            nonces.insert(
                nonce.clone(),
                Nonce {
                    origin: origin.to_owned(),
                    expires: now + NONCE_LIFETIME,
                },
            );
            nonce
        };
        let listener = self.listener().await?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        Ok(format!("http://127.0.0.1:{port}/{nonce}"))
    }

    /// The listener, bound on the first use.
    async fn listener(&self) -> Result<Arc<TcpListener>, String> {
        let mut guard = self.inner.listener.lock().await;
        if let Some(listener) = guard.as_ref() {
            return Ok(listener.clone());
        }
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| error.to_string())?;
        let listener = Arc::new(listener);
        let nonces = Arc::clone(&self.inner.nonces);
        let accepted = listener.clone();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = accepted.accept().await else {
                    break;
                };
                let nonces = Arc::clone(&nonces);
                tokio::spawn(async move {
                    let _ = answer(socket, nonces).await;
                });
            }
        });
        *guard = Some(listener.clone());
        Ok(listener)
    }
}

/// The answer to one request: 204 with CORS for the nonce's origin, else 404 without.
async fn answer(
    mut socket: TcpStream,
    nonces: Arc<Mutex<HashMap<String, Nonce>>>,
) -> std::io::Result<()> {
    let mut request = vec![0u8; 8192];
    let read = socket.read(&mut request).await?;
    let request = String::from_utf8_lossy(&request[..read]).into_owned();
    let mut lines = request.lines();
    let Some(first) = lines.next() else {
        return Ok(());
    };
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let origin = request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("origin")
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_default();
    let nonce = path.trim_start_matches('/');
    let entry = nonces
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(nonce)
        .filter(|entry| entry.expires > Instant::now())
        .map(|entry| entry.origin.clone());
    let response = match entry {
        Some(expected) if expected == origin => match method.as_str() {
            // A preflight (Chrome's private network access asked for one) gets the same answer,
            // allowing the private network request.
            "OPTIONS" => response(
                204,
                &[
                    ("Access-Control-Allow-Origin", origin.as_str()),
                    ("Vary", "Origin"),
                    ("Cache-Control", "no-store"),
                    ("Access-Control-Allow-Methods", "GET"),
                    ("Access-Control-Allow-Private-Network", "true"),
                ],
            ),
            "GET" => response(
                204,
                &[
                    ("Access-Control-Allow-Origin", origin.as_str()),
                    ("Vary", "Origin"),
                    ("Cache-Control", "no-store"),
                ],
            ),
            _ => response(
                405,
                &[
                    ("Access-Control-Allow-Origin", origin.as_str()),
                    ("Vary", "Origin"),
                    ("Cache-Control", "no-store"),
                ],
            ),
        },
        _ => response(404, &[("Cache-Control", "no-store")]),
    };
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await
}

/// The HTTP answer with its headers.
fn response(status: u16, headers: &[(&str, &str)]) -> String {
    let reason = match status {
        204 => "No Content",
        405 => "Method Not Allowed",
        _ => "Not Found",
    };
    let mut answer = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in headers {
        answer.push_str(&format!("{name}: {value}\r\n"));
    }
    answer.push_str("Content-Length: 0\r\nConnection: close\r\n\r\n");
    answer
}

/// The random bytes of a nonce.
fn random_bytes(count: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(count);
    while bytes.len() < count {
        bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    }
    bytes.truncate(count);
    bytes
}

/// The bytes of `bytes` in lower-case hexadecimal.
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = std::fmt::Write::write_fmt(&mut text, format_args!("{byte:02x}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One request to the probe, and its answer.
    async fn request(url: &str, method: &str, origin: Option<&str>) -> String {
        let address = url.trim_start_matches("http://");
        let (host, path) = address.split_once('/').expect("a host and a path");
        let mut socket = TcpStream::connect(host).await.expect("the probe answers");
        let mut head = format!("{method} /{path} HTTP/1.1\r\nHost: {host}\r\n");
        if let Some(origin) = origin {
            head.push_str(&format!("Origin: {origin}\r\n"));
        }
        head.push_str("Connection: close\r\n\r\n");
        socket.write_all(head.as_bytes()).await.expect("a request");
        let mut answer = String::new();
        socket.read_to_string(&mut answer).await.expect("an answer");
        answer
    }

    #[tokio::test]
    async fn the_page_origin_is_answered_and_the_others_are_not() {
        let probe = LoopbackProbe::new();
        let url = probe
            .url("https://page.example")
            .await
            .expect("a probe url");
        let answered = request(&url, "GET", Some("https://page.example")).await;
        assert!(answered.starts_with("HTTP/1.1 204"), "{answered}");
        assert!(
            answered.contains("Access-Control-Allow-Origin: https://page.example"),
            "{answered}"
        );
        let refused = request(&url, "GET", Some("https://other.example")).await;
        assert!(refused.starts_with("HTTP/1.1 404"), "{refused}");
        assert!(
            !refused.contains("Access-Control-Allow-Origin"),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn an_unknown_nonce_is_not_answered() {
        let probe = LoopbackProbe::new();
        let url = probe
            .url("https://page.example")
            .await
            .expect("a probe url");
        let unknown = url.replace(url.rsplit('/').next().unwrap_or_default(), "0000");
        let refused = request(&unknown, "GET", Some("https://page.example")).await;
        assert!(refused.starts_with("HTTP/1.1 404"), "{refused}");
    }

    #[tokio::test]
    async fn a_preflight_is_answered_and_another_method_is_not() {
        let probe = LoopbackProbe::new();
        let url = probe
            .url("https://page.example")
            .await
            .expect("a probe url");
        let preflight = request(&url, "OPTIONS", Some("https://page.example")).await;
        assert!(preflight.starts_with("HTTP/1.1 204"), "{preflight}");
        assert!(
            preflight.contains("Access-Control-Allow-Private-Network: true"),
            "{preflight}"
        );
        let posted = request(&url, "POST", Some("https://page.example")).await;
        assert!(posted.starts_with("HTTP/1.1 405"), "{posted}");
    }
}

//! A small blocking HTTP client for p3's own commands and tests: the `p3` command asks its server for
//! an owner link and for the owner's stop, and the integration tests drive the same routes.
//!
//! It speaks HTTP/1.1 on loopback, sends `Connection: close`, and reads the answer whole.

use std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

/// An answer: its status, its headers, and its body as text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    /// The first header called `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The body as JSON.
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_str(&self.body).ok()
    }
}

/// What a request failed with.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("p3: no port to reach")]
    NoPort,
    #[error("p3: the answer to {0} was malformed")]
    Malformed(String),
}

/// One request's shape.
#[derive(Clone, Copy, Debug, Default)]
pub struct Request<'a> {
    pub bearer: Option<&'a str>,
    pub cookie: Option<&'a str>,
    pub json: Option<&'a str>,
}

/// Posts to `path` on `port`, as `request` says.
pub fn post(port: u16, path: &str, request: Request<'_>) -> Result<Response, ClientError> {
    call("POST", port, path, request)
}

/// Gets `path` on `port`, as `request` says.
pub fn get(port: u16, path: &str, request: Request<'_>) -> Result<Response, ClientError> {
    call("GET", port, path, request)
}

fn call(
    method: &str,
    port: u16,
    path: &str,
    request: Request<'_>,
) -> Result<Response, ClientError> {
    let mut stream = connect(port)?;
    let mut head =
        format!("{method} {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nconnection: close\r\n");
    if let Some(token) = request.bearer {
        head.push_str(&format!("authorization: Bearer {token}\r\n"));
    }
    if let Some(cookie) = request.cookie {
        head.push_str(&format!("cookie: {cookie}\r\n"));
    }
    if let Some(body) = request.json {
        head.push_str("content-type: application/json\r\n");
        head.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    if let Some(body) = request.json {
        head.push_str(body);
    }
    stream.write_all(head.as_bytes())?;
    stream.flush()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    parse(path, &raw)
}

fn connect(port: u16) -> Result<TcpStream, ClientError> {
    if port == 0 {
        return Err(ClientError::NoPort);
    }
    let mut addresses = ("127.0.0.1", port).to_socket_addrs()?;
    let address = addresses.next().ok_or(ClientError::NoPort)?;
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}

fn parse(path: &str, raw: &[u8]) -> Result<Response, ClientError> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| ClientError::Malformed(path.to_string()))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| ClientError::Malformed(path.to_string()))?;
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect();
    Ok(Response {
        status,
        headers,
        body: body.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_parses_its_status_headers_and_body() {
        let raw = b"HTTP/1.1 403 Forbidden\r\ncontent-type: text/plain\r\nset-cookie: a=b; Path=/\r\n\r\nno";
        let response = parse("/x", raw).expect("parse");
        assert_eq!(response.status, 403);
        assert_eq!(response.header("set-cookie"), Some("a=b; Path=/"));
        assert_eq!(response.body, "no");
        assert!(matches!(
            parse("/x", b"rubbish"),
            Err(ClientError::Malformed(_))
        ));
    }

    #[test]
    fn no_port_is_no_request() {
        assert!(matches!(
            post(0, "/_p3/stop", Request::default()),
            Err(ClientError::NoPort)
        ));
    }
}

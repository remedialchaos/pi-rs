//! Loopback OpenAI completions fixture shared by real native Pi integration tests.
use std::{
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use serde_json::json;

pub struct MockModel {
    stopping: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
}

impl MockModel {
    pub fn start(agent: &Path) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        fs::create_dir_all(agent)?;
        fs::write(
            agent.join("models.json"),
            serde_json::to_vec(&json!({"providers":{
                "eval-fixture":{"baseUrl":format!("http://{address}/v1"),"api":"openai-completions",
                    "apiKey":"local-test-key","models":[{"id":"eval-model","input":["text"],"reasoning":false,
                    "contextWindow":100000,"maxTokens":2048}]}
            }}))?,
        )?;
        fs::write(
            agent.join("auth.json"),
            br#"{"eval-fixture":{"type":"api_key","key":"local-test-key"}}"#,
        )?;
        listener.set_nonblocking(true)?;
        let stopping = Arc::new(AtomicBool::new(false));
        let done = stopping.clone();
        let server = thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Err(e) = respond(stream) {
                            eprintln!("Local model request failed: {e}");
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            stopping,
            server: Some(server),
        })
    }
}

impl Drop for MockModel {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn respond(mut stream: TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if !line.starts_with("POST /v1/chat/completions ") {
        stream.write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )?;
        return Ok(());
    }
    let mut length = 0;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().map_err(io::Error::other)?;
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let request: serde_json::Value = serde_json::from_slice(&body)?;
    if request["model"] != "eval-model" {
        return Err(io::Error::other("Unexpected model"));
    }
    let chunk = |delta, finish, usage| {
        json!({"id":"local","object":"chat.completion.chunk","created":1,
        "model":"eval-model","choices":[{"index":0,"delta":delta,"finish_reason":finish}],"usage":usage})
    };
    let text = chunk(
        json!({"role":"assistant","content":"Paris"}),
        json!(null),
        json!(null),
    );
    let finish = chunk(
        json!({}),
        json!("stop"),
        json!({"prompt_tokens":12,"completion_tokens":1,"total_tokens":13}),
    );
    let data = format!("data: {text}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
        data.len()
    )
}

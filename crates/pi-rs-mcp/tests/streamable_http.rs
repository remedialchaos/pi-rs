//! `packages/mcp/test/streamable-http.test.ts`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::HeaderMap;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use pi_rs_mcp::{
    AuthProvider, LATEST_PROTOCOL_VERSION, McpClient, McpClientOptions, McpErrorEvent,
    McpRequestOptions, McpTransport, McpTransportMessageListener, SseEvent,
    StreamableHttpTransport, StreamableHttpTransportOptions, consume_sse_stream,
};

#[path = "support/mod.rs"]
#[expect(
    dead_code,
    reason = "shared test helpers: each integration-test binary uses a different subset"
)]
mod support;

use pi_rs_mcp::{ByteStream, McpFetchError};
use support::{
    TestBody, TestHandler, TestResponse, chunk, close_servers, fake_fetch, fetch_response,
    stream_of_chunks,
};

/// One recorded request of a streamable-HTTP test server.
#[derive(Clone)]
struct Recording {
    requests: Arc<Mutex<Vec<Recorded>>>,
}

#[derive(Clone)]
struct Recorded {
    method: String,
    headers: HeaderMap,
    message: Option<Value>,
}

impl Recording {
    fn push(&self, request: &support::RecordedRequest) {
        self.requests.lock().unwrap().push(Recorded {
            method: request.method.clone(),
            headers: request.headers.clone(),
            message: request.message.clone(),
        });
    }

    fn all(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    fn find_message(&self, method: &str) -> Option<Recorded> {
        self.all().into_iter().find(|entry| {
            entry
                .message
                .as_ref()
                .and_then(|m| m.get("method"))
                .and_then(Value::as_str)
                == Some(method)
        })
    }

    fn any_method(&self, method: &str) -> bool {
        self.all().iter().any(|entry| entry.method == method)
    }
}

/// The upstream `protocolHandler`: initialize and tools/list as JSON, tools/call as SSE.
async fn protocol_handler(
    request: support::RecordedRequest,
    recording: &Recording,
) -> TestResponse {
    if request.method == "GET" {
        recording.push(&request);
        return TestResponse {
            status: 405,
            headers: Vec::new(),
            body: TestBody::Empty,
        };
    }
    if request.method == "DELETE" {
        recording.push(&request);
        return TestResponse {
            status: 200,
            headers: Vec::new(),
            body: TestBody::Empty,
        };
    }
    recording.push(&request);
    let message = request.message.clone().unwrap_or(Value::Null);
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    if message.get("id").is_none() {
        return TestResponse {
            status: 202,
            headers: Vec::new(),
            body: TestBody::Empty,
        };
    }
    match message.get("method").and_then(Value::as_str) {
		Some("initialize") => TestResponse {
			status: 200,
			headers: vec![
				("content-type".into(), "application/json".into()),
				("mcp-session-id".into(), "session-1".into()),
			],
			body: TestBody::Full(
				json!({
					"jsonrpc": "2.0",
					"id": id,
					"result": {
						"protocolVersion": LATEST_PROTOCOL_VERSION,
						"capabilities": { "tools": {} },
						"serverInfo": { "name": "http-fixture", "version": "1.0.0" },
					},
				})
				.to_string(),
			),
		},
		Some("tools/list") => TestResponse {
			status: 200,
			headers: vec![("content-type".into(), "application/json".into())],
			body: TestBody::Full(
				json!({
					"jsonrpc": "2.0",
					"id": id,
					"result": { "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }] },
				})
				.to_string(),
			),
		},
		_ => {
			let (sender, receiver) = mpsc::channel::<Vec<u8>>(4);
			// Like upstream's `response.write`, the chunks leave before the response returns.
			let _ = sender.try_send(chunk(&format!(
				"id: tool-result\ndata: {}\n\n",
				json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [{ "type": "text", "text": "hello" }] } })
			)));
			drop(sender);
			TestResponse {
				status: 200,
				headers: vec![("content-type".into(), "text/event-stream".into())],
				body: TestBody::Chunks(receiver),
			}
		}
	}
}

/// Starts a test server at `<origin>/mcp` recording every request.
async fn start_server(
    handler: Arc<
        dyn Fn(
                support::RecordedRequest,
                Recording,
            ) -> futures_util::future::BoxFuture<'static, TestResponse>
            + Send
            + Sync,
    >,
) -> (String, Recording) {
    let recording = Recording {
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let server_recording = recording.clone();
    let listen_handler: TestHandler = {
        let handler = handler.clone();
        let recording = server_recording.clone();
        Arc::new(move |request| {
            let handler = handler.clone();
            let recording = recording.clone();
            let future: futures_util::future::BoxFuture<'static, TestResponse> =
                Box::pin(async move { handler(request, recording).await });
            future
        })
    };
    let origin = support::TestServer::listen(listen_handler)
        .await
        .origin
        .clone();
    (format!("{origin}/mcp"), recording)
}

fn client() -> McpClient {
    McpClient::new(McpClientOptions {
        name: "http-test".to_string(),
        version: "1.0.0".to_string(),
        ..McpClientOptions::default()
    })
}

/// A stream of the given text chunks, closing at the end.
fn closing_stream(chunks: Vec<String>) -> ByteStream {
    let (sender, receiver) = mpsc::channel::<Vec<u8>>(4);
    tokio::spawn(async move {
        for text in chunks {
            let _ = sender.send(chunk(&text)).await;
        }
    });
    Box::pin(futures_util::StreamExt::map(
        tokio_stream::wrappers::ReceiverStream::new(receiver),
        Ok::<_, McpFetchError>,
    ))
}

async fn collect_events(stream: ByteStream) -> Vec<SseEvent> {
    let events: Arc<Mutex<Vec<SseEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let collected = events.clone();
    consume_sse_stream(
        stream,
        pi_rs_mcp::ConsumeSseOptions {
            max_event_bytes: None,
            on_event: Arc::new(move |event: &SseEvent| {
                collected.lock().unwrap().push(event.clone());
            }),
            on_id: None,
            on_retry: None,
        },
    )
    .await
    .expect("the stream parses");
    events.lock().unwrap().clone()
}

#[tokio::test]
async fn parses_chunked_crlf_events_comments_ids_and_multiline_data() {
    let events = collect_events(closing_stream(vec![
        ": keepalive\r\nid: 7\r\ndata: {\"one\":\r\n".to_string(),
        "data: 1}\r\n\r\n".to_string(),
    ]))
    .await;
    assert_eq!(
        events,
        vec![SseEvent {
            event: None,
            data: "{\"one\":\n1}".to_string(),
            id: Some("7".to_string())
        }]
    );
}

#[tokio::test]
async fn recognizes_cr_lf_and_crlf_line_endings_including_crlf_split_across_chunks() {
    let expected = vec![
        SseEvent {
            event: None,
            data: "a\nb".to_string(),
            id: Some("1".to_string()),
        },
        SseEvent {
            event: None,
            data: "c".to_string(),
            id: None,
        },
    ];
    for separator in ["\n", "\r\n", "\r"] {
        let text = ["id: 1", "data: a", "data: b", "", "data: c", "", ""].join(separator);
        assert_eq!(
            collect_events(closing_stream(vec![text.clone()])).await,
            expected
        );

        let chunks: Vec<String> = text.chars().map(|c| c.to_string()).collect();
        assert_eq!(collect_events(closing_stream(chunks)).await, expected);
    }
    assert_eq!(
        collect_events(closing_stream(vec![
            "id: 1\rdata: a\r\ndata: b\n\r".to_string(),
            "\ndata: c\r\r".to_string(),
        ]))
        .await,
        expected
    );
}

#[tokio::test]
async fn dispatches_cr_terminated_events_while_the_stream_stays_open() {
    let (sender, receiver) = mpsc::channel::<Vec<u8>>(4);
    let stream: ByteStream = Box::pin(futures_util::StreamExt::map(
        tokio_stream::wrappers::ReceiverStream::new(receiver),
        Ok::<_, McpFetchError>,
    ));
    let (first_seen, first_wait) = tokio::sync::oneshot::channel::<SseEvent>();
    let first_seen: Arc<Mutex<Option<tokio::sync::oneshot::Sender<SseEvent>>>> =
        Arc::new(Mutex::new(Some(first_seen)));
    let consumer = tokio::spawn(async move {
        let _ = consume_sse_stream(
            stream,
            pi_rs_mcp::ConsumeSseOptions {
                max_event_bytes: None,
                on_event: Arc::new(move |event: &SseEvent| {
                    if let Some(sender) = first_seen.lock().unwrap().take() {
                        let _ = sender.send(event.clone());
                    }
                }),
                on_id: None,
                on_retry: None,
            },
        )
        .await;
    });
    let _ = sender.send(chunk("data: first\r\r")).await;
    let first = tokio::time::timeout(Duration::from_secs(2), first_wait)
        .await
        .expect("the event arrived while the stream stays open")
        .expect("oneshot");
    assert_eq!(
        first,
        SseEvent {
            event: None,
            data: "first".to_string(),
            id: None
        }
    );
    consumer.abort();
    close_servers();
}

#[tokio::test]
async fn rejects_events_whose_data_lines_exceed_the_limit_without_a_blank_line() {
    let (sender, receiver) = mpsc::channel::<Vec<u8>>(1);
    let stream: ByteStream = Box::pin(futures_util::StreamExt::map(
        tokio_stream::wrappers::ReceiverStream::new(receiver),
        Ok::<_, McpFetchError>,
    ));
    let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let producer = {
        let sent = sent.clone();
        let sender = sender.clone();
        tokio::spawn(async move {
            // Never sends a blank line, so the event is never dispatched.
            for _ in 0..1000 {
                if sender
                    .send(chunk("data: xxxxxxxxxxxxxxxx\n"))
                    .await
                    .is_err()
                {
                    break;
                }
                sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        })
    };
    let error = consume_sse_stream(
        stream,
        pi_rs_mcp::ConsumeSseOptions {
            max_event_bytes: Some(256),
            on_event: Arc::new(|_event: &SseEvent| {}),
            on_id: None,
            on_retry: None,
        },
    )
    .await
    .expect_err("the event exceeds the limit");
    assert_eq!(error, "MCP SSE event exceeds 256 bytes");
    assert!(sent.load(std::sync::atomic::Ordering::SeqCst) < 100);
    producer.abort();
    close_servers();
}

// Regression test for #11729: a failed stream must be cancelled, not left open.
#[tokio::test]
async fn cancels_the_stream_when_an_event_exceeds_the_limit() {
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct Cancelling {
        cancelled: Arc<std::sync::atomic::AtomicBool>,
        chunks: std::collections::VecDeque<Vec<u8>>,
    }
    impl futures_util::Stream for Cancelling {
        type Item = Result<Vec<u8>, McpFetchError>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            match self.chunks.pop_front() {
                Some(chunk) => std::task::Poll::Ready(Some(Ok(chunk))),
                None => {
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            }
        }
    }
    impl Drop for Cancelling {
        fn drop(&mut self) {
            // The server keeps the response open after the oversized event.
            self.cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let stream: ByteStream = Box::pin(Cancelling {
        cancelled: cancelled.clone(),
        chunks: vec![chunk(&format!("data: {}\n\n", "x".repeat(512)))].into(),
    });
    let error = consume_sse_stream(
        stream,
        pi_rs_mcp::ConsumeSseOptions {
            max_event_bytes: Some(128),
            on_event: Arc::new(|_event: &SseEvent| {}),
            on_id: None,
            on_retry: None,
        },
    )
    .await
    .expect_err("the event exceeds the limit");
    assert_eq!(error, "MCP SSE event exceeds 128 bytes");
    assert!(
        cancelled.load(std::sync::atomic::Ordering::SeqCst),
        "the stream was cancelled"
    );
}

#[tokio::test]
async fn handles_json_and_sse_responses_with_session_and_protocol_headers() {
    let (url, recording) = start_server(Arc::new(|request, rec| {
        Box::pin(async move { protocol_handler(request, &rec).await })
    }))
    .await;
    let transport = StreamableHttpTransport::new(StreamableHttpTransportOptions {
        url: url.clone(),
        ..Default::default()
    })
    .expect("transport");
    let client = client();
    client.connect(transport.clone()).await.expect("connect");
    assert_eq!(transport.session_id().as_deref(), Some("session-1"));
    let tools = client
        .list_tools(&McpRequestOptions::default())
        .await
        .expect("listTools");
    assert_eq!(
        tools
            .iter()
            .map(|tool| serde_json::to_value(tool).unwrap_or_default())
            .collect::<Vec<_>>(),
        vec![json!({ "name": "echo", "inputSchema": { "type": "object" } })]
    );
    let result = client
        .call_tool(
            "echo",
            Some(json!({ "text": "hello" })),
            &McpRequestOptions::default(),
        )
        .await
        .expect("callTool");
    assert_eq!(
        serde_json::to_value(&result).unwrap_or_default(),
        json!({ "content": [{ "type": "text", "text": "hello" }] })
    );
    client.close().await;

    let list = recording
        .find_message("tools/list")
        .expect("tools/list was sent");
    assert_eq!(
        list.headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok()),
        Some("session-1")
    );
    assert_eq!(
        list.headers
            .get("mcp-protocol-version")
            .and_then(|v| v.to_str().ok()),
        Some(LATEST_PROTOCOL_VERSION)
    );
    assert!(recording.any_method("GET"));
    assert!(recording.any_method("DELETE"));
    close_servers();
}

#[tokio::test]
async fn classifies_authentication_failures() {
    let (url, _recording) = start_server(Arc::new(|_request, _rec| {
        Box::pin(async move {
            TestResponse {
                status: 401,
                headers: vec![(
                    "www-authenticate".into(),
                    "Bearer resource_metadata=\"https://example.com/meta\"".into(),
                )],
                body: TestBody::Full("login required".to_string()),
            }
        })
    }))
    .await;
    let error = client()
        .connect(
            StreamableHttpTransport::new(StreamableHttpTransportOptions {
                url: url.clone(),
                ..Default::default()
            })
            .expect("transport"),
        )
        .await
        .expect_err("401");
    match error {
        pi_rs_mcp::McpClientError::Transport(pi_rs_mcp::McpTransportError::AuthRequired {
            www_authenticate,
            body,
        }) => {
            assert_eq!(
                www_authenticate.as_deref(),
                Some("Bearer resource_metadata=\"https://example.com/meta\"")
            );
            assert_eq!(body, "login required");
        }
        other => panic!("expected AuthRequired, got {other:?}"),
    }
    close_servers();
}

#[tokio::test]
async fn fails_only_the_request_whose_sse_stream_breaks() {
    let (slow_gate, slow_gate_rx) = tokio::sync::mpsc::channel::<()>(1);
    let slow_gate_rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<()>>> =
        Arc::new(tokio::sync::Mutex::new(slow_gate_rx));
    let (url, _recording) = start_server(Arc::new(move |request, rec| {
        let slow_gate_rx = slow_gate_rx.clone();
        Box::pin(async move {
            if request.method != "POST" {
                return protocol_handler(request, &rec).await;
            }
            let message = request.message.clone().unwrap_or(Value::Null);
            let name = message
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if name == "broken" {
                return TestResponse {
                    status: 200,
                    headers: vec![("content-type".into(), "text/event-stream".into())],
                    body: TestBody::Full("data: not json\n\n".to_string()),
                };
            }
            if name == "slow" {
                let mut gate = slow_gate_rx.lock().await;
                let _ = gate.recv().await;
            }
            protocol_handler(request, &rec).await
        })
    }))
    .await;
    let client = client();
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    client.on_error(Arc::new({
        let errors = errors.clone();
        move |error: &McpErrorEvent| {
            errors.lock().unwrap().push(error.message.clone());
        }
    }));
    client
        .connect(
            StreamableHttpTransport::new(StreamableHttpTransportOptions {
                url: url.clone(),
                open_get_stream: Some(false),
                ..Default::default()
            })
            .expect("transport"),
        )
        .await
        .expect("connect");

    let slow = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .call_tool("slow", None, &McpRequestOptions::default())
                .await
        }
    });
    let broken = client
        .call_tool("broken", None, &McpRequestOptions::default())
        .await
        .expect_err("broken stream");
    assert!(broken.to_string().contains("MCP response stream failed"));
    let _ = slow_gate.try_send(());
    let result = slow.await.expect("join slow").expect("slow callTool");
    assert_eq!(
        serde_json::to_value(&result).unwrap_or_default(),
        json!({ "content": [{ "type": "text", "text": "hello" }] })
    );
    client.close().await;
    assert_eq!(errors.lock().unwrap().len(), 1);
    close_servers();
}

#[tokio::test]
async fn opens_the_get_stream_after_initialization_and_sends_last_event_id_only_when_resuming() {
    let order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let order_for_handler = order.clone();
    let (url, recording) = start_server(Arc::new(move |request, rec| {
        let order = order_for_handler.clone();
        Box::pin(async move {
            if request.method == "POST" {
                let method = request
                    .message
                    .as_ref()
                    .and_then(|message| message.get("method"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                order.lock().unwrap().push(method);
                return protocol_handler(request, &rec).await;
            }
            order.lock().unwrap().push(request.method.clone());
            protocol_handler(request, &rec).await
        })
    }))
    .await;
    let client = client();
    client
        .connect(
            StreamableHttpTransport::new(StreamableHttpTransportOptions {
                url: url.clone(),
                ..Default::default()
            })
            .expect("transport"),
        )
        .await
        .expect("connect");
    client
        .call_tool("echo", None, &McpRequestOptions::default())
        .await
        .expect("callTool");
    client
        .list_tools(&McpRequestOptions::default())
        .await
        .expect("listTools");
    client.close().await;
    let order = order.lock().unwrap().clone();
    let initialized_at = order
        .iter()
        .position(|entry| entry == "notifications/initialized")
        .expect("initialized notification sent");
    assert!(
        order
            .iter()
            .position(|entry| entry == "GET")
            .expect("GET stream opened")
            > initialized_at,
        "the GET stream opened after initialization"
    );
    assert!(
        recording
            .all()
            .iter()
            .all(|entry| entry.headers.get("last-event-id").is_none()),
        "no Last-Event-ID without resumption"
    );
    close_servers();
}

#[tokio::test]
async fn resumes_a_response_stream_the_server_closed_before_answering() {
    let resume_headers: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let resume_for_handler = resume_headers.clone();
    let (url, _recording) = start_server(Arc::new(move |request, rec| {
		let resume_headers = resume_for_handler.clone();
		Box::pin(async move {
			if request.method == "GET" && request.headers.get("last-event-id").is_some() {
				resume_headers.lock().unwrap().push(
					request
						.headers
						.get("last-event-id")
						.and_then(|value| value.to_str().ok())
						.map(str::to_string),
				);
				return TestResponse {
					status: 200,
					headers: vec![("content-type".into(), "text/event-stream".into())],
					body: TestBody::Full(format!(
						"id: 2\ndata: {}\n\n",
						json!({ "jsonrpc": "2.0", "id": 2, "result": { "content": [{ "type": "text", "text": "resumed" }] } })
					)),
				};
			}
			if request.method != "POST" {
				return protocol_handler(request, &rec).await;
			}
			let message = request.message.clone().unwrap_or(Value::Null);
			if message.get("method").and_then(Value::as_str) != Some("tools/call") {
				return protocol_handler(request, &rec).await;
			}
			// Priming event (ID, no data) and a retry hint, then the server drops the stream.
			TestResponse {
				status: 200,
				headers: vec![("content-type".into(), "text/event-stream".into())],
				body: TestBody::Full("id: 1\nretry: 5\ndata:\n\n".to_string()),
			}
		})
	}))
	.await;
    let client = client();
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    client.on_error(Arc::new({
        let errors = errors.clone();
        move |error: &McpErrorEvent| {
            errors.lock().unwrap().push(error.message.clone());
        }
    }));
    client
        .connect(
            StreamableHttpTransport::new(StreamableHttpTransportOptions {
                url: url.clone(),
                open_get_stream: Some(false),
                ..Default::default()
            })
            .expect("transport"),
        )
        .await
        .expect("connect");
    let result = client
        .call_tool("echo", None, &McpRequestOptions::default())
        .await
        .expect("resumed callTool");
    assert_eq!(
        serde_json::to_value(&result).unwrap_or_default(),
        json!({ "content": [{ "type": "text", "text": "resumed" }] })
    );
    assert_eq!(
        resume_headers.lock().unwrap().clone(),
        vec![Some("1".to_string())],
        "the resume carried the priming event's id"
    );
    assert!(errors.lock().unwrap().is_empty());
    client.close().await;
    close_servers();
}

#[tokio::test]
async fn fails_a_request_whose_response_stream_ends_without_an_answer() {
    let (url, _recording) = start_server(Arc::new(|request, rec| {
        Box::pin(async move {
            if request.method != "POST" {
                return protocol_handler(request, &rec).await;
            }
            let message = request.message.clone().unwrap_or(Value::Null);
            if message.get("method").and_then(Value::as_str) != Some("tools/call") {
                return protocol_handler(request, &rec).await;
            }
            TestResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                body: TestBody::Full(": nothing here\n\n".to_string()),
            }
        })
    }))
    .await;
    let client = client();
    client
        .connect(
            StreamableHttpTransport::new(StreamableHttpTransportOptions {
                url: url.clone(),
                open_get_stream: Some(false),
                ..Default::default()
            })
            .expect("transport"),
        )
        .await
        .expect("connect");
    let error = client
        .call_tool(
            "echo",
            Some(json!({})),
            &McpRequestOptions {
                timeout_ms: Some(5_000),
                ..McpRequestOptions::default()
            },
        )
        .await
        .expect_err("no answer");
    assert_eq!(
        error.to_string(),
        "MCP response stream failed: stream ended without a response"
    );
    client.close().await;
    close_servers();
}

#[tokio::test]
async fn reconnects_the_get_stream_after_it_drops() {
    let gets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let last_event_ids: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let gets_for_handler = gets.clone();
    let ids_for_handler = last_event_ids.clone();
    let (url, _recording) = start_server(Arc::new(move |request, rec| {
        let gets = gets_for_handler.clone();
        let last_event_ids = ids_for_handler.clone();
        Box::pin(async move {
            if request.method != "GET" {
                return protocol_handler(request, &rec).await;
            }
            let count = gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            last_event_ids.lock().unwrap().push(
                request
                    .headers
                    .get("last-event-id")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string),
            );
            let notification =
                json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" });
            if count == 1 {
                return TestResponse {
                    status: 200,
                    headers: vec![("content-type".into(), "text/event-stream".into())],
                    body: TestBody::Full(format!("id: g1\ndata: {notification}\n\n")),
                };
            }
            let (sender, receiver) = mpsc::channel::<Vec<u8>>(4);
            let _ = sender.try_send(chunk(&format!("id: g2\ndata: {notification}\n\n")));
            TestResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                body: TestBody::Chunks(receiver),
            }
        })
    }))
    .await;
    let client = client();
    let changes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (second_change_tx, second_change_rx) = tokio::sync::oneshot::channel::<()>();
    let second_change_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>> =
        Arc::new(Mutex::new(Some(second_change_tx)));
    let changes_for_listener = changes.clone();
    client.on_notification(
        "notifications/tools/list_changed",
        Arc::new(move |_params: Option<&Value>| {
            if changes_for_listener.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == 2
                && let Some(sender) = second_change_tx.lock().unwrap().take()
            {
                let _ = sender.send(());
            }
        }),
    );
    client
        .connect(
            StreamableHttpTransport::new(StreamableHttpTransportOptions {
                url: url.clone(),
                reconnect: Some(pi_rs_mcp::StreamableHttpReconnectOptions {
                    initial_delay_ms: Some(1),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .expect("transport"),
        )
        .await
        .expect("connect");
    tokio::time::timeout(Duration::from_secs(5), second_change_rx)
        .await
        .expect("the second list-changed notification arrived")
        .expect("notification");
    assert_eq!(
        last_event_ids.lock().unwrap().clone(),
        vec![None, Some("g1".to_string())]
    );
    client.close().await;
    close_servers();
}

#[tokio::test]
async fn rejects_a_request_the_server_accepts_without_a_response() {
    let (url, _recording) = start_server(Arc::new(|request, rec| {
        Box::pin(async move {
            if request.method != "POST" {
                return protocol_handler(request, &rec).await;
            }
            let message = request.message.clone().unwrap_or(Value::Null);
            if message.get("method").and_then(Value::as_str) != Some("tools/call") {
                return protocol_handler(request, &rec).await;
            }
            TestResponse {
                status: 202,
                headers: Vec::new(),
                body: TestBody::Empty,
            }
        })
    }))
    .await;
    let client = client();
    client
        .connect(
            StreamableHttpTransport::new(StreamableHttpTransportOptions {
                url: url.clone(),
                open_get_stream: Some(false),
                ..Default::default()
            })
            .expect("transport"),
        )
        .await
        .expect("connect");
    let error = client
        .call_tool("echo", None, &McpRequestOptions::default())
        .await
        .expect_err("accepted without a response");
    assert!(error.to_string().contains("without a response"));
    client.close().await;
    close_servers();
}

// #11335
#[tokio::test]
async fn applies_max_message_bytes_to_json_responses() {
    let send = move |description_bytes: usize, expect_error: bool| {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "x".repeat(description_bytes),
                    "inputSchema": {},
                }],
            },
        })
        .to_string();
        let body_text = body;
        let fetch = fake_fetch(Arc::new(move || {
            fetch_response(
                200,
                &[("content-type", "application/json")],
                stream_of_chunks(vec![chunk(&body_text)]),
            )
        }));
        async move {
            let transport = StreamableHttpTransport::new(StreamableHttpTransportOptions {
                url: "http://127.0.0.1/mcp".to_string(),
                max_message_bytes: Some(512),
                open_get_stream: Some(false),
                fetch: Some(fetch),
                ..Default::default()
            })
            .expect("transport");
            let messages: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
            let collected = messages.clone();
            transport.on_message(Arc::new(move |message: &pi_rs_mcp::JsonRpcMessage| {
                collected.lock().unwrap().push(message.to_json());
            }) as McpTransportMessageListener);
            transport.start().await.expect("start");
            let request = pi_rs_mcp::parse_json_rpc_message(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list",
            }))
            .expect("parse");
            let sent = transport.send(&request, &Default::default()).await;
            let count = messages.lock().unwrap().len();
            transport.close().await;
            match sent {
                Ok(()) => {
                    assert!(!expect_error, "expected a size error");
                    assert_eq!(count, 1);
                }
                Err(error) => {
                    assert!(expect_error, "unexpected error: {error}");
                    assert_eq!(error.to_string(), "MCP JSON response exceeds 512 bytes");
                }
            }
        }
    };

    send(32, false).await;
    send(2048, true).await;
    close_servers();
}

#[tokio::test]
async fn decodes_json_responses_split_inside_a_bom_and_a_multi_byte_character() {
    let bytes: Vec<u8> = "\u{FEFF}"
        .as_bytes()
        .iter()
        .chain(
            json!({ "jsonrpc": "2.0", "id": 1, "result": { "text": "é€" } })
                .to_string()
                .as_bytes(),
        )
        .copied()
        .collect();
    let chunks: Vec<Vec<u8>> = bytes.iter().map(|byte| vec![*byte]).collect();
    let fetch = fake_fetch(Arc::new(move || {
        fetch_response(
            200,
            &[("content-type", "application/json")],
            stream_of_chunks(chunks.clone()),
        )
    }));
    let transport = StreamableHttpTransport::new(StreamableHttpTransportOptions {
        url: "http://127.0.0.1/mcp".to_string(),
        open_get_stream: Some(false),
        fetch: Some(fetch),
        ..Default::default()
    })
    .expect("transport");
    let messages: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let collected = messages.clone();
    transport.on_message(Arc::new(move |message: &pi_rs_mcp::JsonRpcMessage| {
        collected.lock().unwrap().push(message.to_json());
    }) as McpTransportMessageListener);
    transport.start().await.expect("start");
    let request = pi_rs_mcp::parse_json_rpc_message(
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .expect("parse");
    transport
        .send(&request, &Default::default())
        .await
        .expect("send");
    transport.close().await;
    assert_eq!(
        messages.lock().unwrap().clone(),
        vec![json!({ "jsonrpc": "2.0", "id": 1, "result": { "text": "é€" } })]
    );
}

#[tokio::test]
async fn reads_only_an_excerpt_of_http_error_bodies_and_cancels_the_rest() {
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fetch = fake_fetch(Arc::new({
        let cancelled = cancelled.clone();
        move || {
            let (sender, receiver) = mpsc::channel::<Vec<u8>>(4);
            let cancelled_for_producer = cancelled.clone();
            tokio::spawn(async move {
                // An endless error body: reading it whole would never finish.
                while sender.send(chunk("x")).await.is_ok() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                cancelled_for_producer.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            let stream: ByteStream = Box::pin(futures_util::StreamExt::map(
                tokio_stream::wrappers::ReceiverStream::new(receiver),
                Ok::<_, McpFetchError>,
            ));
            fetch_response(500, &[], stream)
        }
    }));
    let transport = StreamableHttpTransport::new(StreamableHttpTransportOptions {
        url: "http://127.0.0.1/mcp".to_string(),
        open_get_stream: Some(false),
        fetch: Some(fetch),
        ..Default::default()
    })
    .expect("transport");
    transport.start().await.expect("start");
    let request = pi_rs_mcp::parse_json_rpc_message(
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .expect("parse");
    let error = transport
        .send(&request, &Default::default())
        .await
        .expect_err("500");
    transport.close().await;
    match error {
        pi_rs_mcp::McpTransportError::Http { status, body, .. } => {
            assert_eq!(status, 500);
            assert_eq!(body.len(), 8 * 1024);
        }
        other => panic!("expected Http, got {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        cancelled.load(std::sync::atomic::Ordering::SeqCst),
        "the body was cancelled"
    );
}

#[tokio::test]
async fn includes_the_response_body_in_http_errors() {
    let (url, _recording) = start_server(Arc::new(|_request, _rec| {
        Box::pin(async move {
            TestResponse {
                status: 400,
                headers: Vec::new(),
                body: TestBody::Full("Invalid Accept header".to_string()),
            }
        })
    }))
    .await;
    let error = client()
        .connect(
            StreamableHttpTransport::new(StreamableHttpTransportOptions {
                url: url.clone(),
                ..Default::default()
            })
            .expect("transport"),
        )
        .await
        .expect_err("400");
    assert!(
        error
            .to_string()
            .contains("MCP HTTP request failed with status 400: Invalid Accept header"),
        "error was: {error}"
    );
    close_servers();
}

type SeenResponses = Arc<Mutex<Vec<(u16, Option<String>)>>>;

/// An auth provider the test drives: a mutable token plus what it saw.
#[derive(Clone)]
struct TestAuth {
    token: Arc<Mutex<String>>,
    seen: SeenResponses,
}

impl TestAuth {
    fn new(token: &str) -> Self {
        Self {
            token: Arc::new(Mutex::new(token.to_string())),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait::async_trait]
impl AuthProvider for TestAuth {
    async fn token(&self) -> Result<Option<String>, pi_rs_mcp::AuthError> {
        Ok(Some(self.token.lock().unwrap().clone()))
    }

    async fn on_unauthorized(
        &self,
        context: pi_rs_mcp::UnauthorizedContext,
    ) -> Result<(), pi_rs_mcp::AuthError> {
        let next = match context.status {
            401 => "new",
            _ => "admin",
        };
        self.seen
            .lock()
            .unwrap()
            .push((context.status, context.token.clone()));
        *self.token.lock().unwrap() = next.to_string();
        Ok(())
    }
}

#[tokio::test]
async fn hands_401_and_insufficient_scope_403_responses_to_the_auth_provider_with_the_rejected_token()
 {
    let (url, _recording) = start_server(Arc::new(|request, rec| {
        Box::pin(async move {
            if request.method != "POST" {
                return protocol_handler(request, &rec).await;
            }
            let message = request.message.clone().unwrap_or(Value::Null);
            let authorization = request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            if authorization.as_deref() == Some("Bearer old") {
                return TestResponse {
                    status: 401,
                    headers: vec![("www-authenticate".into(), "Bearer".into())],
                    body: TestBody::Empty,
                };
            }
            if message.get("method").and_then(Value::as_str) == Some("tools/call")
                && authorization.as_deref() == Some("Bearer new")
            {
                return TestResponse {
                    status: 403,
                    headers: vec![(
                        "www-authenticate".into(),
                        "Bearer error=\"insufficient_scope\", scope=\"admin\"".into(),
                    )],
                    body: TestBody::Empty,
                };
            }
            protocol_handler(request, &rec).await
        })
    }))
    .await;
    let auth = TestAuth::new("old");
    let client = client();
    client
        .connect(
            StreamableHttpTransport::new(StreamableHttpTransportOptions {
                url: url.clone(),
                open_get_stream: Some(false),
                auth_provider: Some(Arc::new(auth.clone())),
                ..Default::default()
            })
            .expect("transport"),
        )
        .await
        .expect("connect");
    let result = client
        .call_tool("echo", None, &McpRequestOptions::default())
        .await
        .expect("callTool");
    assert_eq!(
        serde_json::to_value(&result).unwrap_or_default(),
        json!({ "content": [{ "type": "text", "text": "hello" }] })
    );
    assert_eq!(
        auth.seen.lock().unwrap().clone(),
        vec![
            (401, Some("old".to_string())),
            (403, Some("new".to_string()))
        ]
    );
    client.close().await;
    close_servers();
}

// #10598: a request cancelled or timed out while waiting for a shared refresh must not be retried.
#[tokio::test]
async fn does_not_retry_requests_that_were_cancelled_while_authorization_was_recovering() {
    let (release_refresh, refresh_gate) = tokio::sync::watch::channel(false);
    let (all_waiting_tx, mut all_waiting_rx) = tokio::sync::mpsc::channel::<()>(3);
    let executed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let executed_for_handler = executed.clone();
    let all_waiting_for_handler = all_waiting_tx.clone();
    let (url, _recording) = start_server(Arc::new(move |request, rec| {
        let executed = executed_for_handler.clone();
        let all_waiting_tx = all_waiting_for_handler.clone();
        Box::pin(async move {
            if request.method != "POST" {
                return protocol_handler(request, &rec).await;
            }
            let message = request.message.clone().unwrap_or(Value::Null);
            let authorization = request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            if message.get("method").and_then(Value::as_str) == Some("tools/call")
                && authorization.as_deref() == Some("Bearer old")
            {
                return TestResponse {
                    status: 401,
                    headers: vec![("www-authenticate".into(), "Bearer".into())],
                    body: TestBody::Empty,
                };
            }
            if message.get("method").and_then(Value::as_str) == Some("tools/call") {
                let name = message
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                executed.lock().unwrap().push(name);
                let _ = all_waiting_tx.try_send(());
            }
            protocol_handler(request, &rec).await
        })
    }))
    .await;
    let token = Arc::new(Mutex::new("old".to_string()));
    let seen_refreshes = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    #[derive(Clone)]
    struct GatedAuth {
        token: Arc<Mutex<String>>,
        refresh_gate: tokio::sync::watch::Receiver<bool>,
        all_waiting_tx: tokio::sync::mpsc::Sender<()>,
        refreshes: Arc<std::sync::atomic::AtomicUsize>,
        started: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl AuthProvider for GatedAuth {
        async fn token(&self) -> Result<Option<String>, pi_rs_mcp::AuthError> {
            Ok(Some(self.token.lock().unwrap().clone()))
        }

        async fn on_unauthorized(
            &self,
            _context: pi_rs_mcp::UnauthorizedContext,
        ) -> Result<(), pi_rs_mcp::AuthError> {
            // One refresh, shared by every concurrent 401, like upstream's `inFlight` promise.
            if self
                .started
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_ok()
            {
                self.refreshes
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            let _ = self.all_waiting_tx.try_send(());
            let mut gate = self.refresh_gate.clone();
            while !*gate.borrow_and_update() {
                if gate.changed().await.is_err() {
                    break;
                }
            }
            *self.token.lock().unwrap() = "new".to_string();
            Ok(())
        }
    }

    let auth = GatedAuth {
        token: token.clone(),
        refresh_gate: refresh_gate.clone(),
        all_waiting_tx: all_waiting_tx.clone(),
        refreshes: seen_refreshes.clone(),
        started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let client = client();
    client
        .connect(
            StreamableHttpTransport::new(StreamableHttpTransportOptions {
                url: url.clone(),
                open_get_stream: Some(false),
                auth_provider: Some(Arc::new(auth)),
                ..Default::default()
            })
            .expect("transport"),
        )
        .await
        .expect("connect");
    let controller = pi_rs_mcp::AbortController::new();
    let aborted = tokio::spawn({
        let client = client.clone();
        let signal = controller.signal();
        async move {
            client
                .call_tool(
                    "aborted",
                    Some(json!({})),
                    &McpRequestOptions {
                        signal: Some(signal),
                        ..McpRequestOptions::default()
                    },
                )
                .await
        }
    });
    let timed_out = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .call_tool(
                    "timed-out",
                    Some(json!({})),
                    &McpRequestOptions {
                        timeout_ms: Some(200),
                        ..McpRequestOptions::default()
                    },
                )
                .await
        }
    });
    let kept = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .call_tool("kept", None, &McpRequestOptions::default())
                .await
        }
    });
    // All three reached the 401 and are waiting on the shared refresh.
    for _ in 0..3 {
        all_waiting_rx
            .recv()
            .await
            .expect("three calls reached the shared refresh");
    }
    controller.abort(None);
    let error = aborted.await.expect("join aborted").expect_err("aborted");
    assert!(matches!(error, pi_rs_mcp::McpClientError::Aborted));
    let error = timed_out
        .await
        .expect("join timed out")
        .expect_err("timed out");
    assert!(matches!(error, pi_rs_mcp::McpClientError::Timeout { .. }));
    let _ = release_refresh.send(true);
    let result = kept.await.expect("join kept").expect("kept callTool");
    assert_eq!(
        serde_json::to_value(&result).unwrap_or_default(),
        json!({ "content": [{ "type": "text", "text": "hello" }] })
    );
    assert_eq!(executed.lock().unwrap().clone(), vec!["kept".to_string()]);
    assert_eq!(seen_refreshes.load(std::sync::atomic::Ordering::SeqCst), 1);
    client.close().await;
    close_servers();
}

// #10565: the auth provider may refresh over the network, which closing must not wait for.
#[tokio::test]
async fn closes_the_session_with_the_last_requests_token_without_asking_the_auth_provider() {
    let (url, recording) = start_server(Arc::new(|request, rec| {
        Box::pin(async move { protocol_handler(request, &rec).await })
    }))
    .await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    #[derive(Clone)]
    struct CountingAuth {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl AuthProvider for CountingAuth {
        async fn token(&self) -> Result<Option<String>, pi_rs_mcp::AuthError> {
            let number = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            Ok(Some(format!("token-{number}")))
        }
    }
    let auth = CountingAuth {
        calls: calls.clone(),
    };
    let client = client();
    client
        .connect(
            StreamableHttpTransport::new(StreamableHttpTransportOptions {
                url: url.clone(),
                open_get_stream: Some(false),
                auth_provider: Some(Arc::new(auth)),
                ..Default::default()
            })
            .expect("transport"),
        )
        .await
        .expect("connect");
    let before = calls.load(std::sync::atomic::Ordering::SeqCst);
    client.close().await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), before);
    let deletes: Vec<_> = recording
        .all()
        .into_iter()
        .filter(|entry| entry.method == "DELETE")
        .collect();
    assert_eq!(deletes.len(), 1);
    assert_eq!(
        deletes[0]
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some(format!("Bearer token-{before}").as_str())
    );
    assert_eq!(
        deletes[0]
            .headers
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok()),
        Some("session-1")
    );
    close_servers();
}

#[tokio::test]
async fn classifies_an_expired_established_session() {
    let posts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let posts_for_handler = posts.clone();
    let (url, _recording) = start_server(Arc::new(move |request, rec| {
        let posts = posts_for_handler.clone();
        Box::pin(async move {
            if request.method == "POST"
                && posts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 2
            {
                return TestResponse {
                    status: 404,
                    headers: Vec::new(),
                    body: TestBody::Full("gone".to_string()),
                };
            }
            protocol_handler(request, &rec).await
        })
    }))
    .await;
    let client = client();
    client
        .connect(
            StreamableHttpTransport::new(StreamableHttpTransportOptions {
                url: url.clone(),
                open_get_stream: Some(false),
                ..Default::default()
            })
            .expect("transport"),
        )
        .await
        .expect("connect");
    let error = client
        .list_tools(&McpRequestOptions::default())
        .await
        .expect_err("session expired");
    assert!(matches!(
        error,
        pi_rs_mcp::McpClientError::Transport(pi_rs_mcp::McpTransportError::SessionExpired { .. })
    ));
    client.close().await;
    close_servers();
}

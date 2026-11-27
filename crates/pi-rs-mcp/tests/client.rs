//! `packages/mcp/test/client.test.ts`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use serde_json::{Value, json};

use pi_rs_mcp::{
    ClientState, InMemoryTransport, JsonRpcId, JsonRpcMessage, LATEST_PROTOCOL_VERSION, McpClient,
    McpClientError, McpClientOptions, McpError, McpRequestOptions, McpTransport,
    ProgressNotification, Roots, create_in_memory_transport_pair,
};

/// The server side of a client test: what it saw and how it answers.
struct TestServer {
    transport: Arc<InMemoryTransport>,
    messages: Arc<Mutex<Vec<Value>>>,
    handlers: Arc<Mutex<std::collections::HashMap<String, Handler>>>,
}

type Handler = Arc<
    dyn Fn(Value) -> futures_util::future::BoxFuture<'static, Result<Value, McpError>>
        + Send
        + Sync,
>;

async fn create_server() -> (Arc<InMemoryTransport>, Arc<TestServer>) {
    let pair = create_in_memory_transport_pair();
    let handlers: Arc<Mutex<std::collections::HashMap<String, Handler>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let messages: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let transport = pair.server.clone();
        let handlers = handlers.clone();
        let messages = messages.clone();
        let listener_transport = transport.clone();
        listener_transport.on_message(Arc::new(move |message: &JsonRpcMessage| {
			messages.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(message.to_json());
			let Some(request) = message_request(message) else { return };
			let method = request.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
			let id = request.get("id").cloned().unwrap_or(Value::Null);
			let handler = handlers
				.lock()
				.unwrap_or_else(|poisoned| poisoned.into_inner())
				.get(&method)
				.cloned();
			let transport = transport.clone();
			tokio::spawn(async move {
				let reply = match handler {
					None => Err(McpError::new(-32601, format!("Method not found: {method}"))),
					Some(handler) => handler(request.clone()).await,
				};
				let response = match reply {
					Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
					Err(error) => json!({
						"jsonrpc": "2.0",
						"id": id,
						"error": { "code": error.code, "message": error.message, "data": error.data },
					}),
				};
				let _ = transport
					.send(
						&parse_message(&response),
						&pi_rs_mcp::McpTransportSendOptions::default(),
					)
					.await;
			});
		}));
    }
    let _ = pair.server.start().await;
    let server = Arc::new(TestServer {
        transport: pair.server.clone(),
        messages: messages.clone(),
        handlers: handlers.clone(),
    });
    set_handler(
        &server,
        "initialize",
        Arc::new(|_request| {
            async {
                Ok(json!({
                    "protocolVersion": LATEST_PROTOCOL_VERSION,
                    "capabilities": { "tools": { "listChanged": true } },
                    "serverInfo": { "name": "test-server", "version": "1.0.0" },
                    "instructions": "Use test tools.",
                }))
            }
            .boxed()
        }),
    );
    (pair.client, server)
}

fn parse_message(value: &Value) -> JsonRpcMessage {
    pi_rs_mcp::parse_json_rpc_message(value).unwrap_or(JsonRpcMessage::Notification(
        pi_rs_mcp::JsonRpcNotification {
            jsonrpc: pi_rs_mcp::JsonRpcVersion,
            method: String::new(),
            params: None,
            extra: Default::default(),
        },
    ))
}

fn message_request(message: &JsonRpcMessage) -> Option<Value> {
    match message {
        JsonRpcMessage::Request(request) => Some(json!({
            "jsonrpc": "2.0",
            "id": request.id.to_json(),
            "method": request.method,
            "params": request.params.clone().unwrap_or(Value::Null),
        })),
        _ => None,
    }
}

fn set_handler(server: &TestServer, method: &str, handler: Handler) {
    server
        .handlers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(method.to_string(), handler);
}

async fn connect() -> (McpClient, Arc<TestServer>) {
    let (client_transport, server) = create_server().await;
    let client = McpClient::new(McpClientOptions {
        name: "test-client".to_string(),
        version: "2.0.0".to_string(),
        ..McpClientOptions::default()
    });
    client.connect(client_transport).await.expect("connect");
    (client, server)
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(10)).await;
}

#[tokio::test]
async fn initializes_the_connection_before_exposing_server_information() {
    let (client, server) = connect().await;
    assert_eq!(client.connection_state(), ClientState::Connected);
    assert_eq!(
        client.protocol_version().as_deref(),
        Some(LATEST_PROTOCOL_VERSION)
    );
    assert_eq!(
        client
            .server_info()
            .map(|info| serde_json::to_value(info).unwrap_or_default()),
        Some(json!({ "name": "test-server", "version": "1.0.0" }))
    );
    assert_eq!(
        client
            .server_capabilities()
            .map(|capabilities| serde_json::to_value(capabilities).unwrap_or_default()),
        Some(json!({ "tools": { "listChanged": true } }))
    );
    assert_eq!(client.instructions().as_deref(), Some("Use test tools."));
    settle().await;
    let messages = server
        .messages
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert_eq!(
        messages,
        vec![
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": LATEST_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "test-client", "version": "2.0.0" },
                },
            }),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        ]
    );
    client.close().await;
}

#[tokio::test]
async fn paginates_tools_and_preserves_protocol_tool_definitions() {
    let (client, server) = connect().await;
    set_handler(
        &server,
        "tools/list",
        Arc::new(|request| {
            async move {
				let cursor = request
					.pointer("/params/cursor")
					.and_then(Value::as_str)
					.map(str::to_string);
				match cursor {
					None => Ok(json!({
						"tools": [{ "name": "search", "description": "Search", "inputSchema": { "type": "object" } }],
						"nextCursor": "page-2",
					})),
					Some(_) => Ok(json!({
						"tools": [{
							"name": "read",
							"inputSchema": { "type": "object" },
							"outputSchema": { "type": "object" },
							"annotations": { "readOnlyHint": true },
						}],
						// Some servers end pagination with an empty cursor instead of omitting it.
						"nextCursor": "",
					})),
				}
			}
			.boxed()
        }),
    );
    let tools = client
        .list_tools(&McpRequestOptions::default())
        .await
        .expect("listTools");
    let as_json: Vec<Value> = tools
        .iter()
        .map(|tool| serde_json::to_value(tool).unwrap_or_default())
        .collect();
    assert_eq!(
        as_json,
        vec![
            json!({ "name": "search", "description": "Search", "inputSchema": { "type": "object" } }),
            json!({
                "name": "read",
                "inputSchema": { "type": "object" },
                "outputSchema": { "type": "object" },
                "annotations": { "readOnlyHint": true },
            }),
        ]
    );
    client.close().await;
}

#[tokio::test]
async fn lists_and_reads_resources() {
    let (client, server) = connect().await;
    set_handler(
        &server,
        "resources/list",
        Arc::new(|request| {
            async move {
				match request.pointer("/params/cursor").and_then(Value::as_str) {
					None => Ok(json!({
						"resources": [{ "uri": "file:///a", "name": "a", "mimeType": "text/plain" }],
						"nextCursor": "2",
					})),
					Some(_) => Ok(json!({ "resources": [{ "uri": "file:///b" }] })),
				}
			}
			.boxed()
        }),
    );
    set_handler(
        &server,
        "resources/templates/list",
        Arc::new(|_request| {
            async {
				Ok(json!({ "resourceTemplates": [{ "uriTemplate": "repo://{owner}/{repo}", "name": "repo" }] }))
			}
			.boxed()
        }),
    );
    set_handler(
        &server,
        "resources/read",
        Arc::new(|request| {
            async move {
                let uri = request
                    .pointer("/params/uri")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Ok(json!({ "contents": [{ "uri": uri, "text": "hello" }] }))
            }
            .boxed()
        }),
    );
    // A missing name falls back to the URI.
    let resources = client
        .list_resources(&McpRequestOptions::default())
        .await
        .expect("listResources");
    let as_json: Vec<Value> = resources
        .iter()
        .map(|resource| serde_json::to_value(resource).unwrap_or_default())
        .collect();
    assert_eq!(
        as_json,
        vec![
            json!({ "uri": "file:///a", "name": "a", "mimeType": "text/plain" }),
            json!({ "uri": "file:///b", "name": "file:///b" }),
        ]
    );
    let templates = client
        .list_resource_templates(&McpRequestOptions::default())
        .await
        .expect("listResourceTemplates");
    assert_eq!(
        templates
            .iter()
            .map(|t| serde_json::to_value(t).unwrap_or_default())
            .collect::<Vec<_>>(),
        vec![json!({ "uriTemplate": "repo://{owner}/{repo}", "name": "repo" })]
    );
    // Single pages pass the cursor through.
    let page = client
        .list_resources_page(None, &McpRequestOptions::default())
        .await
        .expect("page");
    assert_eq!(page.next_cursor.as_deref(), Some("2"));
    assert_eq!(
        page.resources
            .iter()
            .map(|r| serde_json::to_value(r).unwrap_or_default())
            .collect::<Vec<_>>(),
        vec![json!({ "uri": "file:///a", "name": "a", "mimeType": "text/plain" })]
    );
    let page = client
        .list_resources_page(Some("2"), &McpRequestOptions::default())
        .await
        .expect("page");
    assert_eq!(page.next_cursor, None);
    assert_eq!(
        page.resources
            .iter()
            .map(|r| serde_json::to_value(r).unwrap_or_default())
            .collect::<Vec<_>>(),
        vec![json!({ "uri": "file:///b", "name": "file:///b" })]
    );
    let read = client
        .read_resource("file:///a", &McpRequestOptions::default())
        .await
        .expect("readResource");
    assert_eq!(
        serde_json::to_value(read).unwrap_or_default(),
        json!({ "contents": [{ "uri": "file:///a", "text": "hello" }] })
    );

    set_handler(
        &server,
        "resources/read",
        Arc::new(|_request| async { Ok(json!({ "contents": [{ "uri": "file:///a" }] })) }.boxed()),
    );
    let error = client
        .read_resource("file:///a", &McpRequestOptions::default())
        .await
        .expect_err("invalid contents");
    assert!(
        error
            .to_string()
            .contains("Invalid contents in MCP resources/read result")
    );
    set_handler(
        &server,
        "resources/list",
        Arc::new(|_request| async { Ok(json!({ "resources": [{ "name": "no uri" }] })) }.boxed()),
    );
    let error = client
        .list_resources(&McpRequestOptions::default())
        .await
        .expect_err("invalid entry");
    assert!(
        error
            .to_string()
            .contains("Invalid entry in MCP resources/list result")
    );
    client.close().await;
}

#[tokio::test]
async fn returns_structured_tool_content_and_surfaces_json_rpc_errors() {
    let (client, server) = connect().await;
    set_handler(
        &server,
        "tools/call",
        Arc::new(|request| {
            async move {
				let name = request.pointer("/params/name").and_then(Value::as_str).unwrap_or_default();
				if name == "fail" {
					return Err(McpError::with_data(1234, "tool failed", Some(json!({ "retryable": false }))));
				}
				Ok(json!({
					"content": [{ "type": "text", "text": "ok" }],
					"structuredContent": { "count": request.pointer("/params/arguments/count").cloned().unwrap_or(Value::Null) },
				}))
			}
			.boxed()
        }),
    );
    let result = client
        .call_tool(
            "count",
            Some(json!({ "count": 3 })),
            &McpRequestOptions::default(),
        )
        .await
        .expect("callTool");
    assert_eq!(
        serde_json::to_value(&result).unwrap_or_default(),
        json!({ "content": [{ "type": "text", "text": "ok" }], "structuredContent": { "count": 3 } })
    );
    let error = client
        .call_tool("fail", None, &McpRequestOptions::default())
        .await
        .expect_err("tool failure");
    assert_eq!(
        error,
        McpClientError::Mcp {
            code: 1234,
            message: "tool failed".to_string(),
            data: Some(json!({ "retryable": false }))
        }
    );
    client.close().await;
}

#[tokio::test]
async fn renews_the_timeout_on_progress() {
    let (client, server) = connect().await;
    {
        let transport = server.transport.clone();
        set_handler(
            &server,
            "tools/call",
            Arc::new(move |request| {
                let transport = transport.clone();
                async move {
                    let token = request
                        .pointer("/params/_meta/progressToken")
                        .cloned()
                        .unwrap_or(Value::Null);
                    let transport = transport.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(40)).await;
                        let notification = json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": { "progressToken": token, "progress": 1, "total": 2 },
                        });
                        let _ = transport
                            .send(
                                &parse_message(&notification),
                                &pi_rs_mcp::McpTransportSendOptions::default(),
                            )
                            .await;
                    });
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    Ok(json!({ "content": [{ "type": "text", "text": "done" }] }))
                }
                .boxed()
            }),
        );
    }
    let progress: Arc<Mutex<Vec<ProgressNotification>>> = Arc::new(Mutex::new(Vec::new()));
    let options = McpRequestOptions {
        timeout_ms: Some(50),
        on_progress: Some(Arc::new({
            let progress = progress.clone();
            move |notification: &ProgressNotification| {
                progress
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(notification.clone());
            }
        })),
        ..McpRequestOptions::default()
    };
    let result = client
        .call_tool("slow", Some(json!({})), &options)
        .await
        .expect("callTool");
    assert_eq!(
        serde_json::to_value(&result).unwrap_or_default(),
        json!({ "content": [{ "type": "text", "text": "done" }] })
    );
    let recorded = progress
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].progress_token, JsonRpcId::number(2));
    assert_eq!(recorded[0].progress, 1.0);
    assert_eq!(recorded[0].total, Some(2.0));
    client.close().await;
}

#[tokio::test]
async fn cancels_aborted_and_timed_out_requests() {
    let (client, server) = connect().await;
    set_handler(
        &server,
        "tools/call",
        Arc::new(|_request| {
            async { std::future::pending::<Result<Value, McpError>>().await }.boxed()
        }),
    );
    let controller = pi_rs_mcp::AbortController::new();
    let task_client = client.clone();
    let task_signal = controller.signal();
    let aborted = tokio::spawn(async move {
        task_client
            .call_tool(
                "wait",
                Some(json!({})),
                &McpRequestOptions {
                    signal: Some(task_signal),
                    ..McpRequestOptions::default()
                },
            )
            .await
    });
    // Upstream's call is synchronous up to its first await; the spawned task gives the Rust
    // future that beat, so the request leaves before the abort stops it.
    settle().await;
    controller.abort(Some("stop"));
    let error = aborted
        .await
        .expect("join aborted call")
        .expect_err("aborted");
    assert!(matches!(error, McpClientError::Aborted));
    settle().await;
    let messages = server
        .messages
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert!(messages.contains(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": 2, "reason": "stop" },
    })));

    let error = client
        .call_tool(
            "wait",
            Some(json!({})),
            &McpRequestOptions {
                timeout_ms: Some(5),
                ..McpRequestOptions::default()
            },
        )
        .await
        .expect_err("timeout");
    assert!(matches!(error, McpClientError::Timeout { timeout_ms: 5 }));
    client.close().await;
}

#[tokio::test]
async fn reports_transport_errors_without_failing_pending_requests() {
    let (client_transport, server) = create_server().await;
    let client = McpClient::new(McpClientOptions {
        name: "test-client".to_string(),
        version: "1.0.0".to_string(),
        ..McpClientOptions::default()
    });
    client
        .connect(client_transport.clone())
        .await
        .expect("connect");
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    client.on_error(Arc::new({
        let errors = errors.clone();
        move |error: &pi_rs_mcp::McpErrorEvent| {
            errors
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(error.message.clone());
        }
    }));
    let release: Arc<Mutex<Option<Arc<tokio::sync::Notify>>>> = Arc::new(Mutex::new(None));
    let gate = Arc::new(tokio::sync::Notify::new());
    set_handler(
        &server,
        "tools/call",
        Arc::new({
            let release = release.clone();
            let gate = gate.clone();
            move |_request| {
                {
                    let release = release.clone();
                    let gate = gate.clone();
                    async move {
                        gate.notify_waiters();
                        let notified = release
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .take();
                        match notified {
                            Some(notified) => notified.notified().await,
                            None => std::future::pending::<()>().await,
                        }
                        Ok(json!({ "content": [] }))
                    }
                }
                .boxed()
            }
        }),
    );
    let wait_options = McpRequestOptions::default();
    let call = client.call_tool("wait", None, &wait_options);
    settle().await;
    client_transport.emit_error(pi_rs_mcp::McpErrorEvent::message("stray log line"));
    let notified = Arc::new(tokio::sync::Notify::new());
    {
        let mut release_guard = release
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *release_guard = Some(notified.clone());
    }
    notified.notify_one();
    let result = call.await.expect("callTool");
    assert_eq!(
        serde_json::to_value(&result).unwrap_or_default(),
        json!({ "content": [] })
    );
    assert_eq!(
        errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
        vec!["stray log line".to_string()]
    );
    client.close().await;
}

#[tokio::test]
async fn accepts_servers_that_answer_with_an_older_protocol_version() {
    let (client_transport, server) = create_server().await;
    set_handler(
        &server,
        "initialize",
        Arc::new(|_request| {
            async {
                Ok(json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": { "name": "old-server", "version": "0.1.0" },
                }))
            }
            .boxed()
        }),
    );
    let client = McpClient::new(McpClientOptions {
        name: "test-client".to_string(),
        version: "1.0.0".to_string(),
        ..McpClientOptions::default()
    });
    client.connect(client_transport).await.expect("connect");
    assert_eq!(client.protocol_version().as_deref(), Some("2024-11-05"));
    client.close().await;

    let (client_transport, server) = create_server().await;
    set_handler(
        &server,
        "initialize",
        Arc::new(|_request| {
            async {
                Ok(json!({
                    "protocolVersion": "1999-01-01",
                    "capabilities": {},
                    "serverInfo": { "name": "ancient-server", "version": "0.1.0" },
                }))
            }
            .boxed()
        }),
    );
    let rejected = McpClient::new(McpClientOptions {
        name: "test-client".to_string(),
        version: "1.0.0".to_string(),
        ..McpClientOptions::default()
    });
    let error = rejected
        .connect(client_transport)
        .await
        .expect_err("unsupported version");
    assert!(error.to_string().contains("unsupported protocol version"));
    assert_eq!(rejected.connection_state(), ClientState::Closed);
}

#[tokio::test]
async fn defaults_missing_tool_result_content_to_an_empty_list() {
    let (client, server) = connect().await;
    set_handler(
        &server,
        "tools/call",
        Arc::new(|_request| async { Ok(json!({ "structuredContent": { "ok": true } })) }.boxed()),
    );
    let result = client
        .call_tool("structured", None, &McpRequestOptions::default())
        .await
        .expect("callTool");
    assert_eq!(
        serde_json::to_value(&result).unwrap_or_default(),
        json!({ "content": [], "structuredContent": { "ok": true } })
    );
    set_handler(
        &server,
        "tools/call",
        Arc::new(|_request| async { Ok(json!({ "content": "not a list" })) }.boxed()),
    );
    let error = client
        .call_tool("broken", None, &McpRequestOptions::default())
        .await
        .expect_err("invalid result");
    assert!(error.to_string().contains("Invalid MCP tools/call result"));
    client.close().await;
}

// #10416: MCP 2026-07-28 allows any JSON value as structured content.
#[tokio::test]
async fn accepts_structured_content_that_is_not_an_object() {
    let (client, server) = connect().await;
    for structured_content in [
        json!([{ "id": 1 }, { "id": 2 }]),
        serde_json::Value::Null,
        json!("text"),
        json!(3),
        json!(false),
    ] {
        let structured = structured_content.clone();
        set_handler(
            &server,
            "tools/call",
            Arc::new(move |_request| {
                let structured = structured.clone();
                async move { Ok(json!({ "content": [], "structuredContent": structured })) }.boxed()
            }),
        );
        let result = client
            .call_tool("structured", None, &McpRequestOptions::default())
            .await
            .expect("callTool");
        assert_eq!(
            serde_json::to_value(&result).unwrap_or_default(),
            json!({ "content": [], "structuredContent": structured_content })
        );
    }
    client.close().await;
}

#[tokio::test]
async fn does_not_send_cancelled_for_a_timed_out_initialize() {
    let (client_transport, server) = create_server().await;
    set_handler(
        &server,
        "initialize",
        Arc::new(|_request| {
            async { std::future::pending::<Result<Value, McpError>>().await }.boxed()
        }),
    );
    let client = McpClient::new(McpClientOptions {
        name: "test-client".to_string(),
        version: "1.0.0".to_string(),
        request_timeout_ms: Some(5),
        ..McpClientOptions::default()
    });
    let error = client.connect(client_transport).await.expect_err("timeout");
    assert!(matches!(error, McpClientError::Timeout { .. }));
    settle().await;
    let messages = server
        .messages
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert!(
        !messages
            .iter()
            .any(|message| message.get("method").and_then(Value::as_str)
                == Some("notifications/cancelled"))
    );
}

#[tokio::test]
async fn notifies_close_listeners_once_when_the_transport_drops() {
    let (client, server) = connect().await;
    let closed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    client.on_close(Arc::new({
        let closed = closed.clone();
        move || {
            closed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }));
    set_handler(
        &server,
        "tools/call",
        Arc::new(|_request| {
            async { std::future::pending::<Result<Value, McpError>>().await }.boxed()
        }),
    );
    let _wait_options = McpRequestOptions::default();
    let pending = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .call_tool("wait", None, &McpRequestOptions::default())
                .await
        }
    });
    settle().await;
    server.transport.close().await;
    let error = pending.await.expect("join pending").expect_err("closed");
    assert!(error.to_string().contains("MCP connection closed"));
    assert_eq!(client.connection_state(), ClientState::Closed);
    client.close().await;
    assert_eq!(closed.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn answers_roots_list_and_dispatches_notifications() {
    let (client_transport, server) = create_server().await;
    let client = McpClient::new(McpClientOptions {
        name: "test-client".to_string(),
        version: "1.0.0".to_string(),
        roots: Some(Roots::Fixed(vec![pi_rs_mcp::Root {
            uri: "file:///workspace".to_string(),
            name: Some("workspace".to_string()),
            extra: Default::default(),
        }])),
        ..McpClientOptions::default()
    });
    client.connect(client_transport).await.expect("connect");
    let changed: Arc<Mutex<Vec<Option<Value>>>> = Arc::new(Mutex::new(Vec::new()));
    client.on_notification(
        "notifications/tools/list_changed",
        Arc::new({
            let changed = changed.clone();
            move |params: Option<&Value>| {
                changed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(params.cloned());
            }
        }),
    );
    server
        .transport
        .send(
            &parse_message(&json!({ "jsonrpc": "2.0", "id": "roots", "method": "roots/list" })),
            &Default::default(),
        )
        .await
        .expect("send");
    settle().await;
    let messages = server
        .messages
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert!(messages.contains(&json!({
        "jsonrpc": "2.0",
        "id": "roots",
        "result": { "roots": [{ "uri": "file:///workspace", "name": "workspace" }] },
    })));
    server
        .transport
        .send(
            &parse_message(
                &json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }),
            ),
            &Default::default(),
        )
        .await
        .expect("send");
    settle().await;
    let changed = changed
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert_eq!(changed, vec![None]);
    client.close().await;
}

#[tokio::test]
async fn matches_string_echoed_response_ids_and_progress_tokens() {
    let pair = create_in_memory_transport_pair();
    let seen_progress = Arc::new(Mutex::new(Vec::<ProgressNotification>::new()));
    let server_transport = pair.server.clone();
    pair.server
        .on_message(Arc::new(move |message: &JsonRpcMessage| {
            let Some(request) = message_request(message) else {
                return;
            };
            let method = request
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            let id_str = match &id {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            let progress_token = request
                .pointer("/params/_meta/progressToken")
                .and_then(|value| match value {
                    Value::String(text) => Some(text.clone()),
                    Value::Number(number) => Some(number.to_string()),
                    _ => None,
                });
            let transport = server_transport.clone();
            tokio::spawn(async move {
                let reply = if method == "initialize" {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id_str,
                        "result": {
                            "protocolVersion": LATEST_PROTOCOL_VERSION,
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "string-id-server", "version": "1.0.0" },
                        },
                    })
                } else {
                    if let Some(token) = progress_token {
                        tokio::time::sleep(Duration::from_millis(18)).await;
                        let _ = transport
                            .send(
                                &parse_message(&json!({
                                    "jsonrpc": "2.0",
                                    "method": "notifications/progress",
                                    "params": {
                                        "progressToken": token,
                                        "progress": 1,
                                        "total": 2,
                                    },
                                })),
                                &Default::default(),
                            )
                            .await;
                        tokio::time::sleep(Duration::from_millis(18)).await;
                    }
                    json!({
                        "jsonrpc": "2.0",
                        "id": id_str,
                        "result": { "content": [{ "type": "text", "text": "ok" }] },
                    })
                };
                let _ = transport
                    .send(&parse_message(&reply), &Default::default())
                    .await;
            });
        }));
    let _ = pair.server.start().await;

    let client = McpClient::new(McpClientOptions {
        name: "test-client".to_string(),
        version: "2.0.0".to_string(),
        ..McpClientOptions::default()
    });
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded_errors = errors.clone();
    client.on_error(Arc::new(move |error| {
        recorded_errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(error.message.clone());
    }));
    client.connect(pair.client).await.expect("connect");

    let captured_progress = seen_progress.clone();
    let result = client
        .call_tool(
            "slow",
            None,
            &McpRequestOptions {
                timeout_ms: Some(30),
                on_progress: Some(Arc::new(move |notification| {
                    captured_progress
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(notification.clone());
                })),
                ..Default::default()
            },
        )
        .await
        .expect("call_tool resolves when progress renews timeout");
    assert_eq!(result.content.len(), 1);
    let progress = seen_progress
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert_eq!(progress.len(), 1);
    assert_eq!(progress[0].progress, 1.0);
    assert!(
        errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
    );
    client.close().await;
}

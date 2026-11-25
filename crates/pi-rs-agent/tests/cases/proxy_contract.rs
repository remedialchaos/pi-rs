use super::*;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::Json,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use futures_util::stream;
use pi_rs_ai::{AssistantMessageEvent, CacheRetention, Transport, Usage, normalize_context};
use std::time::Duration;
use tokio::{
    net::TcpListener,
    sync::{Notify, oneshot},
    task::JoinHandle,
    time::timeout,
};

struct Server(String, JoinHandle<std::io::Result<()>>);
impl Drop for Server {
    fn drop(&mut self) {
        self.1.abort();
    }
}

async fn serve(router: Router) -> TestResult<Server> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let task = tokio::spawn(async move { axum::serve(listener, router).await });
    Ok(Server(url, task))
}

fn options(server: &Server) -> TestResult<ProxyStreamOptions> {
    let mut stream = StreamOptions::default();
    stream.request.client = Some(reqwest::Client::builder().no_proxy().build()?);
    Ok(ProxyStreamOptions {
        proxy: ProxyOptions {
            proxy_url: server.0.clone(),
            auth_token: "local-token".into(),
        },
        stream,
    })
}

fn frame(event: Value) -> String {
    format!("data: {event}\r\n\r\n")
}
fn usage() -> Value {
    json!(Usage {
        output: 7.0,
        total_tokens: 7.0,
        ..Usage::default()
    })
}

fn sse(body: Body) -> impl IntoResponse {
    ([("content-type", "text/event-stream")], body)
}

#[tokio::test]
async fn posts_normalized_transcript_and_allowlisted_options_only() -> TestResult {
    let (requests, captured) = oneshot::channel();
    let requests = Arc::new(Mutex::new(Some(requests)));
    let server = serve(Router::new().route(
        "/api/stream",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let requests = requests.clone();
            async move {
                if let Some(send) = locked(&requests).take() {
                    let _ = send.send((headers, body));
                }
                sse(Body::from(frame(
                    json!({"type":"done","reason":"stop","usage":usage()}),
                )))
            }
        }),
    ))
    .await?;
    let mut options = options(&server)?;
    options.stream.temperature = Some(0.25);
    options
        .stream
        .sampling_params
        .insert("topP".into(), json!(0.4));
    options.stream.max_tokens = Some(80);
    options.stream.reasoning = Some(ThinkingLevel::High);
    options.stream.cache_retention = Some(CacheRetention::Short);
    options.stream.session_id = Some("session".into());
    options.stream.disable_continuation = true;
    options
        .stream
        .request
        .headers
        .insert("X-App".into(), Some("value".into()));
    options
        .stream
        .metadata
        .insert("trace".into(), json!({"id":1}));
    options.stream.transport = Some(Transport::Sse);
    options
        .stream
        .thinking_budgets
        .insert(ThinkingLevel::High, 500);
    options.stream.request.max_retry_delay_ms = Some(0);
    options.stream.request.api_key = Some("private-provider-key".into());
    options
        .stream
        .request
        .env
        .insert("LOCAL_ONLY".into(), "secret".into());
    options.stream.specific.insert("local".into(), json!(true));
    let agent = Agent::new(AgentOptions {
        stream_options: options.stream,
        proxy: Some(options.proxy),
        ..AgentOptions::default()
    })?;
    agent.append_system_prompt("replayed prompt");
    agent.set_model(model());
    agent.set_thinking_level(ThinkingLevel::High);
    agent.prompt("hello").await?;
    let (headers, body) = timeout(Duration::from_secs(2), captured).await??;
    assert_eq!(headers["authorization"], "Bearer local-token");
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(body["model"]["id"], model().id);
    assert_eq!(body["context"]["messages"][0]["content"], "replayed prompt");
    assert_eq!(
        body["context"]["messages"][1]["content"][0]["text"],
        "hello"
    );
    assert_eq!(
        body["options"],
        json!({
            "temperature":0.25,"samplingParams":{"topP":0.4},"maxTokens":80,"reasoning":"high","cacheRetention":"short",
            "sessionId":"session","disableContinuation":true,"headers":{"X-App":"value"},"metadata":{"trace":{"id":1}},
            "transport":"sse","thinkingBudgets":{"high":500},"maxRetryDelayMs":0
        })
    );
    assert_eq!(agent.state().error_message, None);
    agent.set_proxy_options(None);
    let (direct, requests) = scripted([answer("direct")]);
    agent.set_stream_fn(direct);
    agent.prompt("second").await?;
    assert_eq!(requests.users(), [["hello", "second"]]);
    Ok(())
}

#[tokio::test]
async fn reconstructs_interleaved_content_signatures_and_tool_metadata() -> TestResult {
    let release = Arc::new(Notify::new());
    let release_handler = release.clone();
    let server = serve(Router::new().route("/api/stream", post(move || {
        let release = release_handler.clone();
        async move {
            let early = [
                json!({"type":"start"}), json!({"type":"future_event","anything":true}),
                json!({"type":"text_start","contentIndex":0}), json!({"type":"thinking_start","contentIndex":1}),
                json!({"type":"text_delta","contentIndex":0,"delta":"π🌳"}),
                json!({"type":"thinking_delta","contentIndex":1,"delta":"reason"}),
                json!({"type":"toolcall_start","contentIndex":2,"id":"lookup|fc_1","toolName":"lookup"}),
                json!({"type":"toolcall_delta","contentIndex":2,"delta":"{\"value\":\"hel"}),
            ].into_iter().map(frame).collect::<String>();
            let at = early.find('🌳').map_or(1, |i| i + 1);
            let first = Bytes::copy_from_slice(&early.as_bytes()[..at]);
            let second = Bytes::copy_from_slice(&early.as_bytes()[at..]);
            let tail = stream::once(async move {
                release.notified().await;
                let mut text = [
                    json!({"type":"thinking_end","contentIndex":1,"contentSignature":"thought-signature"}),
                    json!({"type":"text_end","contentIndex":0,"contentSignature":"text-signature"}),
                    json!({"type":"toolcall_delta","contentIndex":2,"delta":"lo\"}"}),
                    json!({"type":"toolcall_end","contentIndex":2,"toolCall":{"type":"toolCall","id":"lookup|fc_1","name":"lookup","arguments":{"value":"hello"},"namespace":"dynamic_tools","thoughtSignature":"tool-signature"}}),
                ].into_iter().map(frame).collect::<String>();
                text.push_str(&format!("data: {}", json!({"type":"done","reason":"toolUse","usage":usage(),"providerThinkingLevel":"high"})));
                Ok::<_, std::io::Error>(Bytes::from(text))
            });
            sse(Body::from_stream(stream::iter([Ok(first), Ok(second)]).chain(tail)))
        }
    }))).await?;
    let mut events = stream_proxy(
        model(),
        normalize_context(Default::default()),
        options(&server)?,
    );
    let mut deltas = 0;
    while let Some(event) = timeout(Duration::from_secs(2), events.next()).await? {
        if let AssistantMessageEvent::ToolCallDelta { partial, .. } = event {
            if deltas == 0 {
                let partial = partial.read().map_err(|_| "poisoned partial message")?;
                let serialized = serde_json::to_value(&partial.content[2])?;
                assert_eq!(
                    serialized,
                    json!({"type":"toolCall","id":"lookup|fc_1","name":"lookup","arguments":{"value":"hel"}})
                );
                release.notify_one();
            }
            deltas += 1;
        } else if let AssistantMessageEvent::ToolCallEnd { tool_call, .. } = event {
            assert_eq!(tool_call.namespace.as_deref(), Some("dynamic_tools"));
        }
    }
    assert_eq!(deltas, 2);
    let result = events.result().await?;
    assert_eq!(result.stop_reason, StopReason::ToolUse);
    assert_eq!(result.usage.output, 7.0);
    assert_eq!(result.provider_thinking_level.as_deref(), Some("high"));
    assert_eq!(
        serde_json::to_value(&result.content)?,
        json!([
            {"type":"text","text":"π🌳","textSignature":"text-signature"},
            {"type":"thinking","thinking":"reason","thinkingSignature":"thought-signature"},
            {"type":"toolCall","id":"lookup|fc_1","name":"lookup","arguments":{"value":"hello"},"namespace":"dynamic_tools","thoughtSignature":"tool-signature"}
        ])
    );
    Ok(())
}

#[tokio::test]
async fn propagates_http_and_protocol_errors_as_terminal_assistant_events() -> TestResult {
    let cases = [
        (
            StatusCode::FORBIDDEN,
            "{\"error\":\"not permitted\"}".into(),
            "Proxy error: not permitted",
            0.0,
        ),
        (
            StatusCode::BAD_GATEWAY,
            "gateway".into(),
            "Proxy error: 502 Bad Gateway",
            0.0,
        ),
        (
            StatusCode::OK,
            frame(json!({"type":"start"})),
            "Connection closed by proxy server before the response completed",
            0.0,
        ),
        (
            StatusCode::OK,
            frame(json!({"type":"text_delta","contentIndex":0,"delta":"broken"})),
            "Received text_delta for non-text content",
            0.0,
        ),
        (
            StatusCode::OK,
            frame(
                json!({"type":"error","reason":"error","errorMessage":"provider overloaded","usage":usage()}),
            ),
            "provider overloaded",
            7.0,
        ),
    ];
    for (status, body, error, tokens) in cases {
        let server =
            serve(Router::new().route("/api/stream", post(move || async move { (status, body) })))
                .await?;
        let mut events = stream_proxy(
            model(),
            normalize_context(Default::default()),
            options(&server)?,
        );
        let result = timeout(Duration::from_secs(2), events.result()).await??;
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(result.error_message.as_deref(), Some(error));
        assert_eq!(result.usage.output, tokens);
        assert!(matches!(
            events.by_ref().last_terminal().await,
            Some(StopReason::Error)
        ));
    }
    Ok(())
}

trait TerminalEvent: futures_util::Stream<Item = AssistantMessageEvent> + Unpin {
    async fn last_terminal(&mut self) -> Option<StopReason> {
        while let Some(event) = self.next().await {
            if let AssistantMessageEvent::Error { reason, .. }
            | AssistantMessageEvent::Done { reason, .. } = event
            {
                return Some(reason);
            }
        }
        None
    }
}
impl<T: futures_util::Stream<Item = AssistantMessageEvent> + Unpin> TerminalEvent for T {}

#[tokio::test]
async fn abort_interrupts_pending_body_read_and_preserves_streamed_text() -> TestResult {
    let server = serve(Router::new().route(
        "/api/stream",
        post(|| async {
            let start = frame(json!({"type":"start"}))
                + &frame(json!({"type":"text_start","contentIndex":0}))
                + &frame(json!({"type":"text_delta","contentIndex":0,"delta":"part"}));
            sse(Body::from_stream(
                stream::once(async { Ok::<_, std::io::Error>(Bytes::from(start)) })
                    .chain(stream::pending()),
            ))
        }),
    ))
    .await?;
    let options = options(&server)?;
    let signal = options.stream.request.signal.clone();
    let mut events = stream_proxy(model(), normalize_context(Default::default()), options);
    while let Some(event) = timeout(Duration::from_secs(2), events.next()).await? {
        if matches!(event, AssistantMessageEvent::TextDelta { .. }) {
            break;
        }
    }
    signal.cancel();
    let result = timeout(Duration::from_secs(2), events.result()).await??;
    assert_eq!(result.stop_reason, StopReason::Aborted);
    assert_eq!(
        result.error_message.as_deref(),
        Some("Request aborted by user")
    );
    assert_eq!(result.content, [faux_text("part")]);
    Ok(())
}

#[tokio::test]
async fn reconstructs_large_tool_arguments_without_leaking_raw_json_to_content() -> TestResult {
    let payload = "line \"quoted\" \\ end\n".repeat(20_000);
    let args = json!({"path":"a.ts","content":payload});
    let json = args.to_string();
    let mut frames = frame(json!({"type":"start"}))
        + &frame(json!({"type":"toolcall_start","contentIndex":0,"id":"large","toolName":"write"}));
    for chunk in json.as_bytes().chunks(32) {
        frames.push_str(&frame(
            json!({"type":"toolcall_delta","contentIndex":0,"delta":std::str::from_utf8(chunk)?}),
        ));
    }
    frames.push_str(&frame(json!({"type":"toolcall_end","contentIndex":0,"toolCall":{"type":"toolCall","id":"large","name":"write","arguments":args}})));
    frames.push_str(&frame(
        json!({"type":"done","reason":"toolUse","usage":usage()}),
    ));
    let server = serve(Router::new().route(
        "/api/stream",
        post(move || async move { sse(Body::from(frames)) }),
    ))
    .await?;
    let events = stream_proxy(
        model(),
        normalize_context(Default::default()),
        options(&server)?,
    );
    let result = timeout(Duration::from_secs(10), events.result()).await??;
    assert_eq!(
        serde_json::to_value(&result.content[0])?,
        json!({"type":"toolCall","id":"large","name":"write","arguments":args})
    );
    Ok(())
}

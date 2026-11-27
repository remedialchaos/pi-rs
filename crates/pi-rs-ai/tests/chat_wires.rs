mod common;
#[path = "cases/core.rs"]
mod core;
#[path = "cases/media.rs"]
mod media;
#[path = "cases/oauth.rs"]
mod oauth;

use aws_smithy_eventstream::frame::write_message_to;
use aws_smithy_types::event_stream::{Header, HeaderValue, Message as Frame};
use axum::{
    Router,
    extract::ws::{Message as WsMessage, WebSocketUpgrade},
    http::{StatusCode, Version},
    response::IntoResponse,
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use common::{Server, TestResult, context, eventstream, model, plain, sse, stream_options};
use pi_rs_ai::{
    Api, AssistantContent, AssistantMessageEvent, CacheRetention, Context, StopReason,
    ThinkingLevel, Transport, anthropic, bedrock, codex, google, mistral, normalize_context,
    openai_completions, pi_messages, responses,
};

#[tokio::test]
async fn claude_streams_thinking_tools_and_long_cache_cost() -> TestResult {
    let server=Server::start(vec![sse(&[
        json!({"type":"message_start","message":{"id":"a1","model":"claude-local","usage":{"input_tokens":20,"cache_read_input_tokens":3,"cache_creation_input_tokens":10,"cache_creation":{"ephemeral_1h_input_tokens":7}}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"thought 🦀"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","name":"read","id":"t1","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"name\":"}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"foo\"}"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4,"output_tokens_details":{"thinking_tokens":2}}}),
        json!({"type":"message_stop"})
    ],"\r\n")]).await?;
    let model = model(
        Api::AnthropicMessages,
        "anthropic",
        "claude-local",
        &server.url,
    )?;
    let mut opts = stream_options()?;
    opts.cache_retention = Some(CacheRetention::Long);
    let mut stream = anthropic::stream(model, normalize_context(context()?), opts);
    let mut delta_count = 0;
    while let Some(event) = stream.next().await {
        if matches!(event, AssistantMessageEvent::ToolCallDelta { .. }) {
            delta_count += 1;
        }
    }
    let result = stream.result().await?;
    assert_eq!(result.stop_reason, StopReason::ToolUse, "{result:?}");
    assert_eq!(delta_count, 2);
    let AssistantContent::Thinking(thought) = &result.content[0] else {
        return Err("missing Claude thinking".into());
    };
    assert_eq!(thought.thinking_signature.as_deref(), Some("signed"));
    let AssistantContent::ToolCall(call) = &result.content[1] else {
        return Err("missing tool".into());
    };
    assert_eq!(call.arguments["name"], "foo");
    assert_eq!(result.usage.total_tokens, 37.0);
    assert!((result.usage.cost.cache_write - 0.00001775).abs() < 1e-12);
    let captured = server.drain()?;
    let wire = captured[0].json()?;
    assert_eq!(captured[0].path, "/v1/messages?beta=true");
    assert_eq!(captured[0].headers["x-api-key"], "fixture-key");
    assert_eq!(
        wire["messages"][0]["content"][0]["cache_control"]["ttl"],
        "1h"
    );
    Ok(())
}

#[tokio::test]
async fn claude_threads_continue_once_with_only_new_messages() -> TestResult {
    let reply = |id: &str| {
        sse(
            &[
                json!({"type":"message_start","message":{"id":id,"model":"fixture","usage":{}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"reply"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
                json!({"type":"message_stop"}),
            ],
            "\n",
        )
    };
    let server = Server::start(vec![reply("a1"), reply("a2")]).await?;
    let mut model = model(Api::AnthropicMessages, "anthropic", "fixture", &server.url)?;
    model.compat = Some(serde_json::from_value(
        json!({"supportsMessageThreads":true}),
    )?);
    let mut opts = stream_options()?;
    opts.session_id = Some("thread-fixture".into());
    let first = anthropic::stream(model.clone(), normalize_context(context()?), opts.clone())
        .result()
        .await?;
    let Context {
        system_prompt,
        mut messages,
        tools,
    } = context()?;
    messages.push(pi_rs_ai::Message::Assistant((*first).clone()));
    messages.extend(context()?.messages);
    let second = anthropic::stream(
        model,
        normalize_context(Context {
            system_prompt,
            messages,
            tools,
        }),
        opts,
    )
    .result()
    .await?;
    assert_eq!(second.stop_reason, StopReason::Stop, "{second:?}");
    let captured = server.drain()?;
    assert_eq!(captured[0].json()?["thread"]["type"], "create");
    let wire = captured[1].json()?;
    assert_eq!(wire["thread"]["previous_message_id"], "a1");
    assert_eq!(
        wire["messages"].as_array().ok_or("missing messages")?.len(),
        1
    );
    assert!(wire.get("system").is_none());
    anthropic::close_sessions(Some("thread-fixture"));
    Ok(())
}

#[tokio::test]
async fn completions_streams_fragmented_tool_arguments_and_reports_cached_usage() -> TestResult {
    let server=Server::start(vec![sse(&[
        json!({"id":"c1","choices":[{"delta":{"reasoning_content":"plan"},"finish_reason":null}]}),
        json!({"id":"c1","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"x\":"}}]}}]}),
        json!({"id":"c1","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls"}]}),
        json!({"id":"c1","choices":[],"usage":{"prompt_tokens":30,"completion_tokens":5,"total_tokens":35,"prompt_tokens_details":{"cached_tokens":8},"completion_tokens_details":{"reasoning_tokens":2}}})
    ],"\n")]).await?;
    let mut model = model(
        Api::OpenAiCompletions,
        "custom",
        "fixture",
        &format!("{}/v1", server.url),
    )?;
    model.compat = Some(serde_json::from_value(
        json!({"supportsStore":false,"maxTokensField":"max_tokens","supportsDeveloperRole":false}),
    )?);
    let out = openai_completions::stream(model, normalize_context(context()?), stream_options()?)
        .result()
        .await?;
    assert_eq!(out.stop_reason, StopReason::ToolUse, "{out:?}");
    assert_eq!(out.response_id.as_deref(), Some("c1"));
    assert_eq!(out.usage.input, 22.0);
    let token = out
        .content
        .iter()
        .find_map(|c| match c {
            AssistantContent::ToolCall(t) => Some(t),
            _ => None,
        })
        .ok_or("missing completions tool")?;
    assert_eq!(token.arguments["x"], 1);
    assert_eq!(out.usage.reasoning, Some(2.0));
    let requests = server.drain()?;
    assert_eq!(requests[0].path, "/v1/chat/completions");
    assert_eq!(requests[0].json()?["max_tokens"], 1024);
    Ok(())
}

fn response_events(id: &str) -> Vec<Value> {
    vec![
        json!({"type":"response.created","response":{"id":id,"status":"in_progress"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_a","type":"message","role":"assistant","status":"in_progress","content":[]}}),
        json!({"type":"response.content_part.added","output_index":0,"item_id":"msg_a","content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
        json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_a","content_index":0,"delta":"hello"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"id":"msg_a","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hello","annotations":[]}]}}),
        json!({"type":"response.completed","response":{"id":id,"status":"completed","output":[{"id":"msg_a","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hello","annotations":[]}]}],"usage":{"input_tokens":20,"output_tokens":7,"total_tokens":27,"input_tokens_details":{"cached_tokens":2,"cache_write_tokens":3}}}}),
    ]
}

#[tokio::test]
async fn responses_and_azure_emit_same_blocks_but_authorize_and_route_differently() -> TestResult {
    for api in [Api::OpenAiResponses, Api::AzureOpenAiResponses] {
        let server = Server::start(vec![sse(&response_events("res_1"), "\n")]).await?;
        let azure = api == Api::AzureOpenAiResponses;
        let provider = if azure { "azure" } else { "openai" };
        let m = model(
            api,
            provider,
            "gpt-test",
            &format!("{}/{}", server.url, if azure { "openai/v1" } else { "v1" }),
        )?;
        let mut opts = stream_options()?;
        if azure {
            opts.specific
                .insert("azureDeploymentName".into(), json!("deployment"));
        }
        let output = responses::stream(m, normalize_context(context()?), opts)
            .result()
            .await?;
        assert_eq!(output.stop_reason, StopReason::Stop, "{output:?}");
        assert_eq!(output.response_id.as_deref(), Some("res_1"));
        let AssistantContent::Text(text) = &output.content[0] else {
            return Err("missing responses text".into());
        };
        assert_eq!(text.text, "hello");
        assert_eq!(output.usage.cache_read, 2.0);
        let requests = server.drain()?;
        let req = &requests[0];
        assert_eq!(
            req.path,
            if azure {
                "/openai/v1/responses?api-version=v1"
            } else {
                "/v1/responses"
            }
        );
        assert_eq!(
            req.json()?["model"],
            if azure { "deployment" } else { "gpt-test" }
        );
        if azure {
            assert_eq!(req.headers["api-key"], "fixture-key");
        } else {
            assert_eq!(req.headers["authorization"], "Bearer fixture-key");
        }
    }
    Ok(())
}

fn codex_key() -> String {
    let jwt = URL_SAFE_NO_PAD
        .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"fixture-account"}}"#);
    format!("x.{jwt}.x")
}
#[tokio::test]
async fn codex_reuses_socket_with_normalized_cached_delta_and_account_header() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let target = format!("http://{}", listener.local_addr()?);
    let app = Router::new().fallback(get(
        |handle: WebSocketUpgrade, headers: axum::http::HeaderMap| async move {
            if headers
                .get("chatgpt-account-id")
                .and_then(|v| v.to_str().ok())
                != Some("fixture-account")
            {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            handle.on_upgrade(|mut ws| async move {
                for (stage, id) in [(0, "ws_1"), (1, "ws_2")] {
                    let Some(Ok(WsMessage::Text(input))) = ws.next().await else {
                        break;
                    };
                    let Ok(value) = serde_json::from_str::<Value>(&input) else {
                        break;
                    };
                    let valid = value["type"] == "response.create"
                        && value["model"] == "gpt-test"
                        && if stage == 0 {
                            value["previous_response_id"].is_null()
                        } else {
                            value["previous_response_id"] == "ws_1"
                                && value["input"].as_array().is_some_and(|items| {
                                    items.len() == 1
                                        && items[0]["content"][0]["text"] == "and again"
                                })
                        };
                    if !valid {
                        let _ = ws
                            .send(WsMessage::Text(
                                json!({"type":"error","message":"unexpected input prefix"})
                                    .to_string()
                                    .into(),
                            ))
                            .await;
                        break;
                    }
                    let mut events = response_events(id);
                    if let Some(last) = events.last_mut() {
                        last["response"]["output"][0]["content"][0]["annotations"] =
                            json!([{"type":"url_citation","url":"https://example.test"}]);
                    }
                    for event in events {
                        if ws
                            .send(WsMessage::Text(event.to_string().into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            })
        },
    ));
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut opts = stream_options()?;
    opts.request.api_key = Some(codex_key());
    opts.transport = Some(Transport::WebsocketCached);
    opts.session_id = Some("codex-cached-fixture".into());
    opts.request.timeout_ms = Some(1500);
    opts.websocket_connect_timeout_ms = Some(1000);
    opts.request.env.insert("NO_PROXY".into(), "*".into());
    let model = model(
        Api::OpenAiCodexResponses,
        "openai-codex",
        "gpt-test",
        &target,
    )?;
    let output = codex::stream(model.clone(), normalize_context(context()?), opts.clone())
        .result()
        .await?;
    assert_eq!(output.stop_reason, StopReason::Stop, "{output:?}");
    assert_eq!(output.response_id.as_deref(), Some("ws_1"));
    let mut history = context()?;
    history
        .messages
        .push(pi_rs_ai::Message::Assistant((*output).clone()));
    history.messages.push(serde_json::from_value(
        json!({"role":"user","content":"and again","timestamp":1}),
    )?);
    let second = codex::stream(model, normalize_context(history), opts)
        .result()
        .await?;
    codex::close_sessions(Some("codex-cached-fixture"));
    handle.abort();
    assert_eq!(second.stop_reason, StopReason::Stop, "{second:?}");
    assert_eq!(second.response_id.as_deref(), Some("ws_2"));
    Ok(())
}
#[tokio::test]
async fn codex_falls_back_to_sse_and_sends_zstd() -> TestResult {
    let server = Server::start(vec![
        plain(StatusCode::BAD_REQUEST, "no upgrade"),
        sse(&response_events("sse_1"), "\n"),
    ])
    .await?;
    let mut opts = stream_options()?;
    opts.request.api_key = Some(codex_key());
    opts.transport = Some(Transport::Auto);
    opts.websocket_connect_timeout_ms = Some(1000);
    opts.request.env.insert("NO_PROXY".into(), "*".into());
    let output = codex::stream(
        model(
            Api::OpenAiCodexResponses,
            "openai-codex",
            "gpt-test",
            &server.url,
        )?,
        normalize_context(context()?),
        opts,
    )
    .result()
    .await?;
    assert_eq!(output.stop_reason, StopReason::Stop, "{output:?}");
    assert_eq!(output.response_id.as_deref(), Some("sse_1"));
    let reqs = server.drain()?;
    assert_eq!(reqs.len(), 2);
    let wire = &reqs[1];
    assert_eq!(wire.headers["content-encoding"], "zstd");
    assert_eq!(wire.headers["chatgpt-account-id"], "fixture-account");
    let payload: Value = serde_json::from_slice(&zstd::decode_all(wire.body.as_ref())?)?;
    assert_eq!(payload["store"], false);
    Ok(())
}

#[tokio::test]
async fn codex_reports_response_metadata_even_for_terminal_usage_limit() -> TestResult {
    let reply = plain(StatusCode::TOO_MANY_REQUESTS,
        json!({"error":{"code":"usage_limit_reached","plan_type":"PLUS","message":"insufficient_quota"}}).to_string());
    let server = Server::start(vec![reply]).await?;
    let mut options = stream_options()?;
    options.transport = Some(Transport::Sse);
    options.request.api_key = Some(codex_key());
    options.request.max_retries = Some(2);
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0));
    options.request.on_response = Some(std::sync::Arc::new({
        let seen = seen.clone();
        move |response, _| {
            seen.store(response.status, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok(()) })
        }
    }));
    let m = model(
        Api::OpenAiCodexResponses,
        "openai-codex",
        "fixture",
        &server.url,
    )?;
    let output = codex::stream(m, normalize_context(context()?), options)
        .result()
        .await?;
    assert_eq!(output.stop_reason, StopReason::Error);
    assert_eq!(
        output.error_message.as_deref(),
        Some("You have hit your ChatGPT usage limit (plus plan).")
    );
    assert_eq!(seen.load(std::sync::atomic::Ordering::Relaxed), 429);
    assert_eq!(server.drain()?.len(), 1);
    Ok(())
}

fn bedrock_events() -> TestResult<Vec<u8>> {
    let mut frames = Vec::new();
    for (kind, raw) in [
        ("messageStart", json!({"role":"assistant"})),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"text":"A 🦀"}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":1,"delta":{"reasoningContent":{"redactedContent":"AQI="}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":1,"delta":{"reasoningContent":{"redactedContent":"AwQ="}}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex":0})),
        ("contentBlockStop", json!({"contentBlockIndex":1})),
        ("messageStop", json!({"stopReason":"end_turn"})),
        (
            "metadata",
            json!({"usage":{"inputTokens":10,"outputTokens":2,"cacheWriteInputTokens":4,"cacheDetails":[{"ttl":"1h","inputTokens":3}],"totalTokens":16}}),
        ),
    ] {
        let frame = Frame::new(raw.to_string())
            .add_header(Header::new(
                ":message-type",
                HeaderValue::String("event".into()),
            ))
            .add_header(Header::new(":event-type", HeaderValue::String(kind.into())));
        write_message_to(&frame, &mut frames)?;
    }
    Ok(frames)
}
#[tokio::test]
async fn bedrock_signs_and_decodes_fragmented_eventstream_with_bearer_override() -> TestResult {
    let frames = bedrock_events()?;
    let server = Server::start(vec![eventstream(frames.clone()), eventstream(frames)]).await?;
    for (signed, expected) in [(true, "AWS4-HMAC-SHA256"), (false, "Bearer fixture-key")] {
        let mut opts = stream_options()?;
        opts.request
            .env
            .insert("AWS_BEDROCK_FORCE_HTTP1".into(), "1".into());
        opts.request
            .env
            .insert("AWS_REGION".into(), "us-east-1".into());
        if signed {
            opts.request.api_key = None;
            opts.request
                .env
                .insert("AWS_BEDROCK_SKIP_AUTH".into(), "1".into());
        }
        let model = model(
            Api::BedrockConverseStream,
            "amazon-bedrock",
            "anthropic.claude-opus-4-7-v1:0",
            &server.url,
        )?;
        let out = bedrock::stream(model, normalize_context(context()?), opts)
            .result()
            .await?;
        assert_eq!(out.stop_reason, StopReason::Stop, "{out:?}");
        assert_eq!(out.usage.cache_write_1h, Some(3.0));
        let AssistantContent::Thinking(t) = &out.content[1] else {
            return Err("missing redacted Bedrock thinking".into());
        };
        assert_eq!(t.thinking_signature.as_deref(), Some("AQIDBA=="));
        assert_eq!(t.redacted, Some(true));
        let requests = server.drain()?;
        let r = &requests[0];
        assert!(r.headers["authorization"].to_str()?.starts_with(expected));
        assert_eq!(r.version, Version::HTTP_11);
        assert!(r.path.ends_with("/converse-stream"));
        assert!(r.json()?.get("modelId").is_none());
    }
    Ok(())
}

#[tokio::test]
async fn gemini_and_vertex_preserve_thought_signature_and_tools() -> TestResult {
    for api in [Api::GoogleGenerativeAi, Api::GoogleVertex] {
        let server=Server::start(vec![sse(&[json!({"responseId":"g-1","candidates":[{"content":{"parts":[{"text":"why","thought":true,"thoughtSignature":"AQI="},{"functionCall":{"id":"lookup-1","name":"lookup","args":{"x":3}}}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":25,"cachedContentTokenCount":8,"candidatesTokenCount":2,"thoughtsTokenCount":5,"totalTokenCount":32}})],"\n")]).await?;
        let vertex = api == Api::GoogleVertex;
        let provider = if vertex { "google-vertex" } else { "google" };
        let m = model(
            api,
            provider,
            "gemini-3-flash",
            &format!("{}/{}", server.url, if vertex { "v1" } else { "v1beta" }),
        )?;
        let mut opts = stream_options()?;
        opts.reasoning = Some(ThinkingLevel::Low);
        let out = google::stream_simple(m, normalize_context(context()?), opts)?
            .result()
            .await?;
        assert_eq!(out.stop_reason, StopReason::ToolUse, "{out:?}");
        assert_eq!(out.usage.input, 17.0);
        assert_eq!(out.usage.output, 7.0);
        let AssistantContent::Thinking(t) = &out.content[0] else {
            return Err("missing Gemini thinking".into());
        };
        assert_eq!(t.thinking_signature.as_deref(), Some("AQI="));
        let r = server.drain()?;
        assert_eq!(r[0].headers["x-goog-api-key"], "fixture-key");
        assert_eq!(
            r[0].path,
            if vertex {
                "/v1/publishers/google/models/gemini-3-flash:streamGenerateContent?alt=sse"
            } else {
                "/v1beta/models/gemini-3-flash:streamGenerateContent?alt=sse"
            }
        );
        let body = r[0].json()?;
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be concise");
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "LOW"
        );
    }
    Ok(())
}

#[tokio::test]
async fn mistral_changes_wire_case_and_preserves_session_affinity() -> TestResult {
    let server=Server::start(vec![sse(&[
        json!({"data":{"id":"m1","choices":[{"delta":{"content":[{"type":"thinking","thinking":[{"text":"plan"}]},{"type":"text","text":"ok"}]},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_token_details":{"cached_tokens":6}}}})
    ],"\n")]).await?;
    let mut opts = stream_options()?;
    opts.session_id = Some("affinity".into());
    opts.reasoning = Some(ThinkingLevel::High);
    let m = model(
        Api::MistralConversations,
        "mistral",
        "mistral-test",
        &server.url,
    )?;
    let out = mistral::stream_simple(m, normalize_context(context()?), opts)?
        .result()
        .await?;
    assert_eq!(out.stop_reason, StopReason::Stop, "{out:?}");
    assert_eq!(out.usage.cache_read, 6.0);
    assert_eq!(out.content.len(), 2);
    let r = server.drain()?;
    assert_eq!(r[0].path, "/v1/chat/completions");
    assert_eq!(r[0].headers["x-affinity"], "affinity");
    let body = r[0].json()?;
    assert_eq!(body["prompt_cache_key"], "affinity");
    assert_eq!(body["prompt_mode"], "reasoning");
    Ok(())
}

#[tokio::test]
async fn pi_messages_preserves_server_rewrites_and_rejects_sparse_indices() -> TestResult {
    let usage = json!({"input":3,"output":2,"totalTokens":5,"cacheRead":0,"cacheWrite":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}});
    let server=Server::start(vec![sse(&[json!({"type":"start"}),json!({"type":"text_start","contentIndex":0}),json!({"type":"text_delta","contentIndex":0,"delta":"prefix"}),
        json!({"type":"text_end","contentIndex":0,"content":"complete","contentSignature":"sig"}),json!({"type":"done","reason":"stop","usage":usage,"responseId":"p1","rewrite":{"policyId":"demo","changed":true}})],"\r\n"),
        sse(&[json!({"type":"text_start","contentIndex":400000000})],"\n")]).await?;
    let m = model(Api::PiMessages, "radius", "native", &server.url)?;
    let mut opts = stream_options()?;
    opts.specific.insert("debug".into(), json!(true));
    let out = pi_messages::stream(m.clone(), normalize_context(context()?), opts.clone())
        .result()
        .await?;
    assert_eq!(out.stop_reason, StopReason::Stop, "{out:?}");
    assert_eq!(out.response_id.as_deref(), Some("p1"));
    let AssistantContent::Text(t) = &out.content[0] else {
        return Err("missing Pi Messages text".into());
    };
    assert_eq!(t.text, "complete");
    assert_eq!(t.text_signature.as_deref(), Some("sig"));
    assert_eq!(
        out.diagnostics.as_ref().ok_or("missing rewrite")?[0]["details"]["policyId"],
        "demo"
    );
    let failed = pi_messages::stream(m, normalize_context(context()?), opts)
        .result()
        .await?;
    assert!(
        failed
            .error_message
            .as_deref()
            .is_some_and(|s| s.contains("Invalid contentIndex"))
    );
    assert!(failed.content.is_empty());
    let captures = server.drain()?;
    assert_eq!(captures[0].path, "/messages?debug=1");
    assert_eq!(captures[0].json()?["options"]["maxTokens"], 1024);
    Ok(())
}

#[tokio::test]
async fn mistral_times_out_after_sixty_seconds_waiting_for_response_headers() -> TestResult {
    use tokio::io::AsyncReadExt;
    use tokio::time::{Duration, advance, pause, timeout};

    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let base = format!("http://{}", listener.local_addr()?);
    let m = model(Api::MistralConversations, "mistral", "mistral-test", &base)?;
    let mut options = stream_options()?;
    options.request.timeout_ms = None;
    let stream = mistral::stream_simple(m, normalize_context(context()?), options)?;
    let (mut peer, _) = listener.accept().await?;
    peer.read_u8().await?;
    pause();
    advance(Duration::from_secs(60)).await;
    let end = timeout(Duration::from_secs(1), stream.result()).await??;
    assert_eq!(end.stop_reason, StopReason::Error);
    assert_eq!(
        end.error_message.as_deref(),
        Some("Mistral response headers timed out after 60000ms")
    );
    Ok(())
}

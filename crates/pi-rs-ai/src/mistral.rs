use indexmap::{IndexMap, IndexSet};
use reqwest::Method;
use serde_json::{Value, json};

use crate::chat_state::ChatState;
use crate::constrained::strict_parameters;
use crate::error::{Error, Result};
use crate::event_stream::{AssistantMessageEventStream, write_partial};
use crate::http::{HttpRequest, abortable, endpoint, payload, request_headers, send};
use crate::model::{CacheRetention, Model, ModelInputModality, ThinkingLevel};
use crate::openai_compat::short_hash;
use crate::request::{ProviderHeaders, StreamOptions};
use crate::simple_options::build_base_options;
use crate::sse::SseReader;
use crate::transcript::{
    get_current_tools, input_text, render_system_message_update, resolve_transcript,
    system_message_text,
};
use crate::transform::transform_messages;
use crate::types::{
    AssistantContent, ImageContent, InputContent, Message, StopReason, StringOrBlocks,
    ThinkingContent, ToolCall, TranscriptContext,
};

fn auth<'a>(model: &Model, opts: &'a StreamOptions) -> Result<&'a str> {
    opts.request
        .api_key
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Auth(format!("No API key for provider: {}", model.provider)))
}
pub fn stream(
    model: Model,
    context: TranscriptContext,
    opts: StreamOptions,
) -> AssistantMessageEventStream {
    let (mut state, stream) = ChatState::new(&model);
    tokio::spawn(async move {
        let result = run(&mut state, &model, context, &opts).await;
        state.finish(result);
    });
    stream
}
pub fn stream_simple(
    model: Model,
    context: TranscriptContext,
    opts: StreamOptions,
) -> Result<AssistantMessageEventStream> {
    auth(&model, &opts)?;
    let reasoning = opts
        .reasoning
        .map(|r| model.clamp_thinking_level(r))
        .filter(|r| *r != ThinkingLevel::Off);
    let mut opts = build_base_options(&model, &context, opts);
    if model.reasoning {
        if let Some(map) = &model.thinking_level_map {
            let effort = if let Some(level) = reasoning {
                Some(map.get(&level).and_then(Option::as_deref).unwrap_or("high"))
            } else {
                map.get(&ThinkingLevel::Off).and_then(Option::as_deref)
            };
            if let Some(effort) = effort {
                opts.specific
                    .insert("reasoningEffort".into(), json!(effort));
            }
        } else if reasoning.is_some() {
            opts.specific
                .insert("promptMode".into(), json!("reasoning"));
        }
    }
    Ok(stream(model, context, opts))
}
fn derived_id(id: &str, attempt: usize) -> String {
    let short = id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>();
    if attempt == 0 && short.len() == 9 {
        return short;
    }
    let seed = if short.is_empty() { id } else { &short };
    let hash = if attempt == 0 {
        short_hash(seed)
    } else {
        short_hash(&format!("{seed}:{attempt}"))
    };
    hash.chars()
        .filter(char::is_ascii_alphanumeric)
        .take(9)
        .collect()
}
fn image(i: &ImageContent) -> Value {
    json!({"type":"image_url","imageUrl":format!("data:{};base64,{}",i.mime_type,i.data)})
}
fn messages(messages: Vec<Message>, images: bool) -> Vec<Value> {
    let mut result = Vec::with_capacity(messages.len());
    for (i, message) in messages.into_iter().enumerate() {
        match message {
            Message::System(s) => {
                let text = if i == 0 {
                    system_message_text(&s)
                } else {
                    render_system_message_update(&s)
                };
                if !text.is_empty() {
                    result.push(json!({"role":"system","content":text}));
                }
            }
            Message::User(u) => match u.content {
                StringOrBlocks::String(s) => result.push(json!({"role":"user","content":s})),
                StringOrBlocks::Blocks(a) => {
                    let mut had_image = false;
                    let blocks: Vec<_> = a
                        .into_iter()
                        .filter_map(|b| match b {
                            InputContent::Text(t) => Some(json!({"type":"text","text":t.text})),
                            InputContent::Image(i) => {
                                had_image = true;
                                images.then(|| image(&i))
                            }
                        })
                        .collect();
                    if !blocks.is_empty() {
                        result.push(json!({"role":"user","content":blocks}));
                    } else if had_image && !images {
                        result.push(json!({"role":"user","content":"(image omitted: model does not support images)"}));
                    }
                }
            },
            Message::Assistant(a) => {
                let mut parts = Vec::new();
                let mut tools = Vec::new();
                for block in a.content {
                    match block{
                AssistantContent::Text(t) if !t.text.trim().is_empty()=>parts.push(json!({"type":"text","text":t.text})),
                AssistantContent::Thinking(t) if !t.thinking.trim().is_empty()=>parts.push(json!({"type":"thinking","thinking":[{"type":"text","text":t.thinking}]})),
                AssistantContent::ToolCall(t)=>tools.push(json!({"id":t.id,"type":"function","function":{"name":t.name,"arguments":Value::Object(t.arguments).to_string()},"index":0})),
                _=>{}
            }
                }
                if !parts.is_empty() || !tools.is_empty() {
                    let mut wire = json!({"role":"assistant","prefix":false});
                    if !parts.is_empty() {
                        wire["content"] = json!(parts);
                    }
                    if !tools.is_empty() {
                        wire["toolCalls"] = json!(tools);
                    }
                    result.push(wire);
                }
            }
            Message::ToolResult(t) => {
                let text = input_text(&t.content, "\n");
                let text = text.trim();
                let mut raw_images = t
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        InputContent::Image(i) => Some(i),
                        _ => None,
                    })
                    .peekable();
                let with_images = raw_images.peek().is_some();
                let prefix = if t.is_error { "[tool error] " } else { "" };
                let output = if !text.is_empty() {
                    format!(
                        "{prefix}{text}{}",
                        if with_images && !images {
                            "\n[tool image omitted: model does not support images]"
                        } else {
                            ""
                        }
                    )
                } else {
                    format!(
                        "{prefix}{}",
                        match (with_images, images) {
                            (true, true) => "(see attached image)",
                            (true, false) => "(image omitted: model does not support images)",
                            _ => "(no tool output)",
                        }
                    )
                };
                let mut content = vec![json!({"type":"text","text":output})];
                if images {
                    content.extend(raw_images.map(image));
                }
                result.push(json!({"role":"tool","toolCallId":t.tool_call_id,"name":t.tool_name,"content":content}));
            }
        }
    }
    result
}
fn to_wire(mut body: Value) -> Value {
    fn remap(value: &mut Value, fields: &[(&str, &str)]) {
        if let Some(o) = value.as_object_mut() {
            for &(from, to) in fields {
                if let Some(value) = o.shift_remove(from) {
                    o.insert(to.into(), value);
                }
            }
        }
    }
    remap(
        &mut body,
        &[
            ("topP", "top_p"),
            ("maxTokens", "max_tokens"),
            ("randomSeed", "random_seed"),
            ("responseFormat", "response_format"),
            ("toolChoice", "tool_choice"),
            ("presencePenalty", "presence_penalty"),
            ("frequencyPenalty", "frequency_penalty"),
            ("parallelToolCalls", "parallel_tool_calls"),
            ("reasoningEffort", "reasoning_effort"),
            ("promptMode", "prompt_mode"),
            ("promptCacheKey", "prompt_cache_key"),
            ("safePrompt", "safe_prompt"),
        ],
    );
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for m in messages {
            remap(
                m,
                &[("toolCalls", "tool_calls"), ("toolCallId", "tool_call_id")],
            );
            if let Some(a) = m.get_mut("content").and_then(Value::as_array_mut) {
                for chunk in a {
                    remap(
                        chunk,
                        &[
                            ("imageUrl", "image_url"),
                            ("documentUrl", "document_url"),
                            ("documentName", "document_name"),
                            ("fileId", "file_id"),
                            ("referenceIds", "reference_ids"),
                            ("inputAudio", "input_audio"),
                        ],
                    );
                }
            }
        }
    }
    if let Some(r) = body.get_mut("response_format") {
        remap(r, &[("jsonSchema", "json_schema")]);
        if let Some(s) = r.get_mut("json_schema") {
            remap(s, &[("schemaDefinition", "schema")]);
        }
    }
    body
}
fn build(
    model: &Model,
    ctx: TranscriptContext,
    opts: &StreamOptions,
    session: Option<&str>,
) -> Result<Value> {
    let ctx = resolve_transcript(
        ctx,
        model
            .compat("supportsMidConvoSystemMessages")
            .as_bool()
            .unwrap_or_default(),
    );
    let tools=get_current_tools(ctx.messages()).into_iter().map(|t|{
        let(params,strict)=strict_parameters(t,model.compat("supportsStrictMode").as_bool().unwrap_or_default(),|_,_|false)?;
        Ok(json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":params,"strict":strict.unwrap_or(false)}}))
    }).collect::<Result<Vec<_>>>()?;
    let mut ids = IndexMap::<String, String>::new();
    let mut used = IndexSet::<String>::new();
    let transformed = transform_messages(ctx.into_messages(), model, |id, _, _| {
        if let Some(existing) = ids.get(id) {
            return Some(existing.clone());
        }
        let candidate = (0..)
            .map(|attempt| derived_id(id, attempt))
            .find(|value| !used.contains(value))?;
        used.insert(candidate.clone());
        ids.insert(id.into(), candidate.clone());
        Some(candidate)
    });
    let mut wire = json!({"model":model.id,"stream":true,"messages":messages(transformed,model.input.contains(&ModelInputModality::Image))});
    if !tools.is_empty() {
        wire["tools"] = json!(tools);
    }
    if let Some(n) = opts.temperature {
        wire["temperature"] = json!(n);
    }
    if let Some(n) = opts.max_tokens {
        wire["maxTokens"] = json!(n);
    }
    if let Some(choice) = opts.specific.get("toolChoice") {
        wire["toolChoice"] = choice.clone();
    } else if let Some(choice) = opts.tool_choice {
        wire["toolChoice"] = json!(choice);
    }
    for field in ["promptMode", "reasoningEffort"] {
        if let Some(v) = opts
            .specific
            .get(field)
            .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        {
            wire[field] = v.clone();
        }
    }
    if let Some(session) = session {
        wire["promptCacheKey"] = json!(session);
    }
    Ok(wire)
}
async fn run(
    state: &mut ChatState,
    model: &Model,
    ctx: TranscriptContext,
    opts: &StreamOptions,
) -> Result<StopReason> {
    let key = auth(model, opts)?;
    let session = opts
        .session_id
        .as_deref()
        .filter(|s| !s.is_empty() && opts.cache_retention != Some(CacheRetention::None));
    let body = to_wire(payload(build(model, ctx, opts, session)?, model, &opts.request).await?);
    let mut headers = ProviderHeaders::from([
        ("authorization".into(), Some(format!("Bearer {key}"))),
        ("accept".into(), Some("text/event-stream".into())),
        ("content-type".into(), Some("application/json".into())),
    ]);
    if let Some(session) = session {
        headers.insert("x-affinity".into(), Some(session.into()));
    }
    let mut request = opts.request.clone();
    request.max_retries = Some(0);
    let response = send(
        HttpRequest {
            method: Method::POST,
            url: endpoint(&model.base_url, "v1/chat/completions")?,
            headers: request_headers(&model.base, &opts.request, headers)?,
            body: Some(serde_json::to_vec(&body)?),
            error_label: "Mistral API error".into(),
            force_http1: false,
            default_retries: 0,
            no_retry_statuses: vec![],
        },
        model,
        &request,
    )
    .await
    .map_err(|e| match e {
        Error::Timeout(t) => Error::Protocol(format!(
            "Mistral response headers timed out after {}ms",
            t.as_millis()
        )),
        e => e,
    })?;
    state.start();
    let mut sse = SseReader::new(response.bytes_stream());
    let mut current = None;
    let mut tools = IndexMap::new();
    let mut finish = None;
    while let Some(e) = sse.next(&opts.request.signal).await? {
        if e.data.trim() == "[DONE]" {
            break;
        }
        if e.data.trim().is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(&e.data)?;
        let chunk = event.get("data").unwrap_or(&event);
        if let Some(watch) = &opts.on_provider_stream_event {
            abortable(&opts.request.signal, watch(chunk, model)).await?;
        }
        let choice = &chunk["choices"][0];
        {
            let mut msg = write_partial(&state.message);
            if msg.response_id.is_none() {
                msg.response_id = chunk["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
            }
            if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
                let p = usage["prompt_tokens"].as_f64().unwrap_or_default();
                let cached = [
                    "/promptTokensDetails/cachedTokens",
                    "/prompt_tokens_details/cached_tokens",
                    "/promptTokenDetails/cachedTokens",
                    "/prompt_token_details/cached_tokens",
                    "/numCachedTokens",
                    "/num_cached_tokens",
                ]
                .into_iter()
                .find_map(|path| usage.pointer(path).and_then(Value::as_f64))
                .unwrap_or_default()
                .max(0.0)
                .min(p);
                msg.usage.input = (p - cached).max(0.0);
                msg.usage.output = usage["completion_tokens"].as_f64().unwrap_or_default();
                msg.usage.cache_read = cached;
                msg.usage.cache_write = 0.0;
                msg.usage.total_tokens = usage["total_tokens"]
                    .as_f64()
                    .filter(|n| *n != 0.0)
                    .unwrap_or(p + msg.usage.output);
                msg.usage.calculate_cost(&model.cost);
            }
            if let Some(raw) = choice["finish_reason"].as_str().filter(|s| !s.is_empty()) {
                msg.raw_stop_reason = Some(raw.into());
                finish = Some(match raw {
                    "stop" => Ok(StopReason::Stop),
                    "length" | "model_length" => Ok(StopReason::Length),
                    "tool_calls" => Ok(StopReason::ToolUse),
                    "error" => Err(Error::Protocol(
                        "Provider stopped with: error (server error)".into(),
                    )),
                    r => Err(Error::Protocol(format!("Provider stopped with: {r}"))),
                });
            }
        }
        let delta = &choice["delta"];
        let mut part = |item: &Value| -> Result<()> {
            let (thinking, text): (bool, String) = if let Some(s) = item.as_str() {
                (false, s.into())
            } else {
                match item["type"].as_str() {
                    Some("thinking") => (
                        true,
                        item["thinking"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|p| p["text"].as_str())
                            .collect(),
                    ),
                    Some("text") => (false, item["text"].as_str().unwrap_or_default().into()),
                    _ => return Ok(()),
                }
            };
            if text.is_empty() {
                return Ok(());
            }
            let index = match current {
                Some((index, was)) if was == thinking => index,
                old => {
                    if let Some((i, _)) = old {
                        state.end(i);
                    }
                    let i = if thinking {
                        state.thinking_start(ThinkingContent::default())
                    } else {
                        state.text_start(None)
                    };
                    current = Some((i, thinking));
                    i
                }
            };
            state.delta(index, text)
        };
        if let Some(parts) = delta["content"].as_array() {
            for item in parts {
                part(item)?;
            }
        } else if delta["content"].is_string() {
            part(&delta["content"])?;
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for tool in calls {
                if let Some((i, _)) = current.take() {
                    state.end(i);
                }
                let id = tool["id"]
                    .as_str()
                    .filter(|s| !matches!(*s, "" | "null"))
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        derived_id(
                            &format!("toolcall:{}", tool["index"].as_u64().unwrap_or(0)),
                            0,
                        )
                    });
                let key = tool["index"]
                    .as_u64()
                    .map(|idx| format!("#{idx}"))
                    .unwrap_or_else(|| id.clone());
                let f = &tool["function"];
                let i = *tools.entry(key).or_insert_with(|| {
                    state.tool_start(ToolCall {
                        id,
                        name: f["name"].as_str().unwrap_or_default().into(),
                        ..ToolCall::default()
                    })
                });
                let args = f.get("arguments").filter(|r| !r.is_null());
                let raw = match args {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => "{}".into(),
                };
                state.delta(i, raw)?;
            }
        }
    }
    if let Some((i, _)) = current {
        state.end(i);
    }
    for i in tools.into_values() {
        state.end(i);
    }
    finish.ok_or_else(|| Error::Protocol("Mistral stream ended without a finish reason".into()))?
}

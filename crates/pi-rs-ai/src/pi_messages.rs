use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::event_stream::{
    AssistantMessageEventSender, AssistantMessageEventStream, fail, live, read_partial,
    write_partial,
};
use crate::http::{
    PROVIDER_TIMEOUT_MS, abortable, client_for, endpoint, header_map, identity_headers, payload,
};
use crate::model::{CacheRetention, Model};
use crate::request::{
    ProviderHeaders, ProviderResponse, StreamOptions, merge_headers, provider_env_value,
};
use crate::sse::SseReader;
use crate::streaming_json::ToolCallArgumentsStream;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent as Event, JsonObject, PartialMessage,
    StopReason, TextContent, ThinkingContent, ToolCall, TranscriptContext, Usage, unix_millis,
};

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
enum Wire {
    Start,
    TextStart {
        content_index: usize,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    TextEnd {
        content_index: usize,
        content: String,
        content_signature: Option<String>,
    },
    ThinkingStart {
        content_index: usize,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
        content_signature: Option<String>,
        redacted: Option<bool>,
    },
    #[serde(rename = "toolcall_start")]
    ToolStart {
        content_index: usize,
        id: String,
        tool_name: String,
    },
    #[serde(rename = "toolcall_delta")]
    ToolDelta {
        content_index: usize,
        delta: String,
    },
    #[serde(rename = "toolcall_end")]
    ToolEnd {
        content_index: usize,
        tool_call: ToolCall,
    },
    Done {
        #[serde(flatten)]
        terminal: Terminal,
    },
    Error {
        #[serde(flatten)]
        terminal: Terminal,
        error_message: Option<String>,
    },
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Terminal {
    reason: StopReason,
    usage: Usage,
    response_id: Option<String>,
    response_model: Option<String>,
    provider_thinking_level: Option<String>,
    rewrite: Option<JsonObject>,
}

pub fn stream(
    model: Model,
    context: TranscriptContext,
    opts: StreamOptions,
) -> AssistantMessageEventStream {
    let (send, events) = AssistantMessageEventStream::channel();
    tokio::spawn(async move {
        let mut failure = None;
        let result = run(&send, &model, context, &opts, &mut failure).await;
        let message = match result {
            Ok(message) => message,
            Err(e) => {
                let mut msg = fail(AssistantMessage::pending(&model), e);
                if let Some(d) = failure {
                    msg.diagnostics = Some(vec![d]);
                }
                msg
            }
        };
        send.finish(message);
    });
    events
}
async fn run(
    send: &AssistantMessageEventSender,
    model: &Model,
    ctx: TranscriptContext,
    opts: &StreamOptions,
    failure: &mut Option<JsonObject>,
) -> Result<AssistantMessage> {
    let key = opts
        .request
        .api_key
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Error::Auth(format!(
                "No API key provided for provider \"{}\"",
                model.provider
            ))
        })?;
    let mut url = endpoint(&model.base_url, "messages")?;
    if opts.specific.get("debug") == Some(&Value::Bool(true)) {
        url.query_pairs_mut().append_pair("debug", "1");
    }
    let retention = opts.cache_retention.or_else(|| {
        (provider_env_value("PI_CACHE_RETENTION", &opts.request.env).as_deref() == Some("long"))
            .then_some(CacheRetention::Long)
    });
    let mut options = serde_json::Map::new();
    for (name, value) in [
        ("temperature", json!(opts.temperature)),
        ("maxTokens", json!(opts.max_tokens)),
        ("reasoning", json!(opts.reasoning)),
        ("cacheRetention", json!(retention)),
        ("sessionId", json!(opts.session_id)),
        (
            "toolChoice",
            opts.specific
                .get("toolChoice")
                .cloned()
                .unwrap_or_else(|| json!(opts.tool_choice)),
        ),
    ] {
        if !value.is_null() {
            options.insert(name.into(), value);
        }
    }
    let wire = payload(
        json!({"model":model.id,"context":ctx,"options":options}),
        model,
        &opts.request,
    )
    .await?;
    let headers = merge_headers(
        identity_headers(&model.base, &opts.request.identity),
        ProviderHeaders::from([
            ("authorization".into(), Some(format!("Bearer {key}"))),
            ("accept".into(), Some("text/event-stream".into())),
            ("content-type".into(), Some("application/json".into())),
        ]),
    );
    let headers = header_map(merge_headers(headers, opts.request.headers.clone()))?;
    let client = client_for(&url, &opts.request, false)?;
    let timeout = Duration::from_millis(opts.request.timeout_ms.unwrap_or(PROVIDER_TIMEOUT_MS));
    let request = client.post(url.clone()).headers(headers).json(&wire);
    let response = abortable(&opts.request.signal, async {
        Ok(tokio::time::timeout(timeout, request.send())
            .await
            .map_err(|_| Error::Timeout(timeout))??)
    })
    .await?;
    let status = response.status();
    if let Some(observer) = &opts.request.on_response {
        let response = ProviderResponse {
            status: status.as_u16(),
            headers: response
                .headers()
                .iter()
                .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_owned())))
                .collect(),
        };
        abortable(&opts.request.signal, observer(&response, model)).await?;
    }
    if !status.is_success() {
        let status_text = status.canonical_reason().unwrap_or_default();
        let body = abortable(&opts.request.signal, async { Ok(response.text().await?) }).await?;
        let structured = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|doc| doc.get("error").filter(|v| v.is_object()).cloned());
        let message = structured
            .as_ref()
            .and_then(|e| e["message"].as_str())
            .unwrap_or(&body);
        let code = structured.as_ref().and_then(|e| e["code"].as_str());
        let suffix = code
            .filter(|s| !s.is_empty())
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        let mut details = json!({"version":1,"provider":model.provider,"model":model.id,"url":url.as_str(),"status":status.as_u16(),"statusText":status_text,"timestampMs":unix_millis()});
        if let Some(doc) = structured.as_ref() {
            details["error"] = doc.clone();
        } else {
            let shortened = body.chars().take(8192).collect::<String>();
            details["body"] = json!(if shortened.len() < body.len() {
                shortened + "…"
            } else {
                shortened
            });
        }
        let message = format!("{} {status_text}: {message}{suffix}", status.as_u16());
        let diagnostic = json!({"type":"pi_messages_response_failure","timestamp":unix_millis(),"error":{"name":"PiMessagesResponseError","code":code,"message":message},"details":details});
        if let Value::Object(d) = diagnostic {
            *failure = Some(d);
        }
        return Err(Error::Protocol(message));
    }
    let partial = live(AssistantMessage::pending(model));
    let mut arguments = HashMap::<usize, ToolCallArgumentsStream>::new();
    let mut frames = SseReader::new(response.bytes_stream());
    while let Some(frame) = frames.next(&opts.request.signal).await? {
        if matches!(frame.data.trim(), "" | "[DONE]") {
            continue;
        }
        let raw: Value = serde_json::from_str(&frame.data)?;
        if raw.is_null() {
            continue;
        }
        if let Some(watch) = &opts.on_provider_stream_event {
            abortable(&opts.request.signal, watch(&raw, model)).await?;
        }
        let valid_index = || -> Result<()> {
            let Some(index) = raw.get("contentIndex") else {
                return Ok(());
            };
            let kind = raw["type"].as_str().unwrap_or_default();
            let len = read_partial(&partial).content.len();
            if index
                .as_u64()
                .and_then(|v| usize::try_from(v).ok())
                .is_some_and(|i| i < len || i == len && kind.ends_with("_start"))
            {
                Ok(())
            } else {
                Err(Error::Protocol(format!(
                    "Invalid contentIndex {index} in pi-messages {kind} event"
                )))
            }
        };
        valid_index()?;
        let event: Wire = serde_json::from_value(raw)?;
        match event {
            Wire::Done { terminal } => return Ok(complete(&partial, terminal, None)),
            Wire::Error {
                terminal,
                error_message,
            } => return Ok(complete(&partial, terminal, error_message)),
            other => send.send(update(&partial, &mut arguments, other)?),
        }
    }
    Err(Error::Protocol(format!(
        "{} stream ended without a terminal event",
        model.provider
    )))
}
fn complete(
    partial: &PartialMessage,
    terminal: Terminal,
    error: Option<String>,
) -> AssistantMessage {
    let mut msg = write_partial(partial);
    msg.stop_reason = terminal.reason;
    msg.usage = terminal.usage;
    msg.response_id = terminal.response_id;
    if terminal.response_model.is_some() {
        msg.response_model = terminal.response_model;
    }
    if terminal.provider_thinking_level.is_some() {
        msg.provider_thinking_level = terminal.provider_thinking_level;
    }
    msg.error_message = error;
    if let Some(rewrite) = terminal.rewrite {
        let d = json!({"type":"pi_messages_rewrite","timestamp":unix_millis(),"details":rewrite});
        if let Value::Object(d) = d {
            msg.diagnostics.get_or_insert_with(Vec::new).push(d);
        }
    }
    msg.clone()
}
fn update(
    partial: &PartialMessage,
    args: &mut HashMap<usize, ToolCallArgumentsStream>,
    wire: Wire,
) -> Result<Event> {
    let mut msg = write_partial(partial);
    let content = &mut msg.content;
    let partial = partial.clone();
    macro_rules! set {
        ($index:expr,$block:expr) => {
            if $index == content.len() {
                content.push($block)
            } else {
                content[$index] = $block
            }
        };
    }
    Ok(match wire {
        Wire::Start => Event::Start { partial },
        Wire::TextStart { content_index } => {
            set!(
                content_index,
                AssistantContent::Text(TextContent::default())
            );
            Event::TextStart {
                content_index,
                partial,
            }
        }
        Wire::TextDelta {
            content_index,
            delta,
        } => {
            if let Some(AssistantContent::Text(t)) = content.get_mut(content_index) {
                t.text.push_str(&delta);
            }
            Event::TextDelta {
                content_index,
                delta,
                partial,
            }
        }
        Wire::TextEnd {
            content_index,
            content: text,
            content_signature,
        } => {
            if let Some(AssistantContent::Text(t)) = content.get_mut(content_index) {
                t.text.clone_from(&text);
                t.text_signature = content_signature;
            }
            Event::TextEnd {
                content_index,
                content: text,
                partial,
            }
        }
        Wire::ThinkingStart { content_index } => {
            set!(
                content_index,
                AssistantContent::Thinking(ThinkingContent::default())
            );
            Event::ThinkingStart {
                content_index,
                partial,
            }
        }
        Wire::ThinkingDelta {
            content_index,
            delta,
        } => {
            if let Some(AssistantContent::Thinking(t)) = content.get_mut(content_index) {
                t.thinking.push_str(&delta);
            }
            Event::ThinkingDelta {
                content_index,
                delta,
                partial,
            }
        }
        Wire::ThinkingEnd {
            content_index,
            content: text,
            content_signature,
            redacted,
        } => {
            if let Some(AssistantContent::Thinking(t)) = content.get_mut(content_index) {
                t.thinking.clone_from(&text);
                t.thinking_signature = content_signature;
                t.redacted = redacted;
            }
            Event::ThinkingEnd {
                content_index,
                content: text,
                partial,
            }
        }
        Wire::ToolStart {
            content_index,
            id,
            tool_name,
        } => {
            set!(
                content_index,
                AssistantContent::ToolCall(ToolCall {
                    id,
                    name: tool_name,
                    ..ToolCall::default()
                })
            );
            args.insert(content_index, ToolCallArgumentsStream::default());
            Event::ToolCallStart {
                content_index,
                partial,
            }
        }
        Wire::ToolDelta {
            content_index,
            delta,
        } => {
            if let Some(AssistantContent::ToolCall(call)) = content.get_mut(content_index)
                && let Some(args) = args.get_mut(&content_index)
                && let Some(snapshot) = args.append(&delta)
                && let Some(value) = snapshot.as_object()
            {
                call.arguments.clone_from(value);
            }
            Event::ToolCallDelta {
                content_index,
                delta,
                partial,
            }
        }
        Wire::ToolEnd {
            content_index,
            tool_call,
        } => {
            set!(content_index, AssistantContent::ToolCall(tool_call.clone()));
            args.remove(&content_index);
            Event::ToolCallEnd {
                content_index,
                tool_call,
                partial,
            }
        }
        Wire::Done { .. } | Wire::Error { .. } => {
            return Err(Error::Protocol(
                "Unexpected pi-messages terminal event".into(),
            ));
        }
    })
}

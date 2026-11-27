use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};

use pi_rs_ai::{
    AssistantContent, AssistantMessage, AssistantMessageEvent as Event,
    AssistantMessageEventSender, AssistantMessageEventStream, Model, PartialMessage, StopReason,
    StreamOptions, ThinkingContent, ToolCall, TranscriptContext, Usage, http::abortable,
    streaming_json::ToolCallArgumentsStream,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::StreamFn;

/// Connection and credentials for the service's `/api/stream` route.
#[derive(Clone)]
pub struct ProxyOptions {
    pub proxy_url: String,
    pub auth_token: String,
}

#[derive(Clone)]
pub struct ProxyStreamOptions {
    pub proxy: ProxyOptions,
    pub stream: StreamOptions,
}

/// Wire events sent by the proxy, without the redundant partial assistant message.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ProxyAssistantMessageEvent {
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
        #[serde(skip_serializing_if = "Option::is_none")]
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
        #[serde(skip_serializing_if = "Option::is_none")]
        content_signature: Option<String>,
    },
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        content_index: usize,
        id: String,
        tool_name: String,
    },
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        content_index: usize,
        delta: String,
    },
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        content_index: usize,
        tool_call: ToolCall,
    },
    Done {
        reason: StopReason,
        usage: Usage,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_thinking_level: Option<String>,
    },
    Error {
        reason: StopReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
        usage: Usage,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_thinking_level: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

pub fn proxy_stream_fn(proxy: ProxyOptions) -> StreamFn {
    Arc::new(move |model, context, stream| {
        let stream = stream_proxy(
            model,
            context,
            ProxyStreamOptions {
                proxy: proxy.clone(),
                stream,
            },
        );
        Box::pin(async move { Ok(stream) })
    })
}

pub fn stream_proxy(
    model: Model,
    context: TranscriptContext,
    options: ProxyStreamOptions,
) -> AssistantMessageEventStream {
    let (tx, events) = AssistantMessageEventStream::channel();
    tokio::spawn(async move {
        let mut partial = ProxyPartial {
            message: Arc::new(RwLock::new(AssistantMessage::pending(&model))),
            tool_arguments: HashMap::new(),
        };
        if let Err(error) = run_proxy(&model, &context, &options, &mut partial, &tx).await {
            let mut message = partial.message.write().unwrap_or_else(|e| e.into_inner());
            if options.stream.request.signal.is_cancelled() {
                message.stop_reason = StopReason::Aborted;
                message.error_message = Some("Request aborted by user".into());
            } else {
                message.stop_reason = StopReason::Error;
                message.error_message = Some(error.to_string());
            }
        }
        let message = partial
            .message
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        tx.finish(message);
    });
    events
}

/// Only these provider options go over the wire; credentials, identity, callbacks and signal stay local.
fn wire_options(options: &StreamOptions) -> pi_rs_ai::Result<Value> {
    let mut wire = serde_json::Map::new();
    macro_rules! field {
        ($key:literal, $value:expr) => {
            if let Some(value) = $value {
                wire.insert($key.into(), serde_json::to_value(value)?);
            }
        };
    }
    field!("temperature", options.temperature);
    field!(
        "samplingParams",
        (!options.sampling_params.is_empty()).then_some(&options.sampling_params)
    );
    field!("maxTokens", options.max_tokens);
    field!("reasoning", options.reasoning);
    field!("cacheRetention", options.cache_retention);
    field!("sessionId", options.session_id.as_ref());
    // `disableContinuation`, `headers` and `metadata` are always sent, like upstream's
    // `buildProxyRequestOptions`: the proxy server reads them as present, not as absent.
    wire.insert(
        "disableContinuation".into(),
        Value::Bool(options.disable_continuation),
    );
    wire.insert(
        "headers".into(),
        serde_json::to_value(&options.request.headers)?,
    );
    wire.insert("metadata".into(), serde_json::to_value(&options.metadata)?);
    field!("transport", options.transport);
    field!(
        "thinkingBudgets",
        (!options.thinking_budgets.is_empty()).then_some(&options.thinking_budgets)
    );
    field!("maxRetryDelayMs", options.request.max_retry_delay_ms);
    Ok(Value::Object(wire))
}

async fn run_proxy(
    model: &Model,
    context: &TranscriptContext,
    options: &ProxyStreamOptions,
    partial: &mut ProxyPartial,
    tx: &AssistantMessageEventSender,
) -> pi_rs_ai::Result<()> {
    #[derive(Serialize)]
    struct Request<'a> {
        model: &'a Model,
        context: &'a TranscriptContext,
        options: Value,
    }
    static CLIENT: LazyLock<std::result::Result<reqwest::Client, reqwest::Error>> =
        LazyLock::new(|| reqwest::Client::builder().build());
    let ProxyStreamOptions { proxy, stream } = options;
    let signal = &stream.request.signal;
    let client = match &stream.request.client {
        Some(client) => client,
        None => CLIENT
            .as_ref()
            .map_err(|error| pi_rs_ai::Error::Protocol(error.to_string()))?,
    };
    let request = client
        .post(format!("{}/api/stream", proxy.proxy_url))
        .bearer_auth(&proxy.auth_token)
        .json(&Request {
            model,
            context,
            options: wire_options(stream)?,
        });
    let mut response = abortable(signal, async { Ok(request.send().await?) }).await?;
    let status = response.status();
    if !status.is_success() {
        let data = abortable(signal, async { Ok(response.json::<Value>().await?) }).await;
        let message = match data {
            Ok(data) => data
                .get("error")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            Err(pi_rs_ai::Error::Aborted) => return Err(pi_rs_ai::Error::Aborted),
            Err(_) => None,
        };
        let description = message.unwrap_or_else(|| {
            format!(
                "{} {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or_default()
            )
        });
        return Err(pi_rs_ai::Error::Protocol(format!(
            "Proxy error: {description}"
        )));
    }

    // Upstream handles each data line independently, including adjacent lines and a final line
    // without LF. Keep incomplete bytes together so UTF-8 split across HTTP chunks is intact.
    let mut tail = Vec::new();
    let mut first_line = true;
    while let Some(chunk) = abortable(signal, async { Ok(response.chunk().await?) }).await? {
        let mut lines = chunk.split_inclusive(|&b| b == b'\n');
        if let Some(line) = lines.next() {
            tail.extend_from_slice(line);
            if line.ends_with(b"\n") {
                if partial.line(&tail, &mut first_line, tx)? {
                    return Ok(());
                }
                tail.clear();
            }
        }
        for line in lines {
            if line.ends_with(b"\n") {
                if partial.line(line, &mut first_line, tx)? {
                    return Ok(());
                }
            } else {
                tail.extend_from_slice(line);
            }
        }
    }
    if signal.is_cancelled() {
        return Err(pi_rs_ai::Error::Aborted);
    }
    if partial.line(&tail, &mut first_line, tx)? {
        return Ok(());
    }
    Err(pi_rs_ai::Error::Protocol(
        "Connection closed by proxy server before the response completed".into(),
    ))
}

struct ProxyPartial {
    message: PartialMessage,
    tool_arguments: HashMap<usize, ProxyToolArguments>,
}

#[derive(Default)]
struct ProxyToolArguments {
    parser: ToolCallArgumentsStream,
    displayed: Option<Arc<Value>>,
}

impl ProxyPartial {
    fn line(
        &mut self,
        line: &[u8],
        first_line: &mut bool,
        tx: &AssistantMessageEventSender,
    ) -> pi_rs_ai::Result<bool> {
        if line.is_empty() {
            return Ok(false);
        }
        let line = if std::mem::take(first_line) {
            line.strip_prefix(b"\xef\xbb\xbf").unwrap_or(line)
        } else {
            line
        };
        let Some(data) = line.strip_prefix(b"data: ") else {
            return Ok(false);
        };
        let data = String::from_utf8_lossy(data);
        if data.trim().is_empty() {
            return Ok(false);
        }
        self.process(serde_json::from_str(data.trim())?, tx)
    }

    fn process(
        &mut self,
        event: ProxyAssistantMessageEvent,
        tx: &AssistantMessageEventSender,
    ) -> pi_rs_ai::Result<bool> {
        use ProxyAssistantMessageEvent as Wire;
        let partial = self.message.clone();
        let mut message = self.message.write().unwrap_or_else(|e| e.into_inner());
        let blocks = &mut message.content;
        let event = match event {
            Wire::Start => Event::Start { partial },
            Wire::TextStart { content_index } => {
                set_block(
                    blocks,
                    content_index,
                    AssistantContent::Text(Default::default()),
                )?;
                Event::TextStart {
                    content_index,
                    partial,
                }
            }
            Wire::TextDelta {
                content_index,
                delta,
            } => {
                let Some(AssistantContent::Text(block)) = blocks.get_mut(content_index) else {
                    return Err(wrong_type("text_delta", "text"));
                };
                block.text.push_str(&delta);
                Event::TextDelta {
                    content_index,
                    delta,
                    partial,
                }
            }
            Wire::TextEnd {
                content_index,
                content_signature,
            } => {
                let Some(AssistantContent::Text(block)) = blocks.get_mut(content_index) else {
                    return Err(wrong_type("text_end", "text"));
                };
                block.text_signature = content_signature;
                Event::TextEnd {
                    content_index,
                    content: block.text.clone(),
                    partial,
                }
            }
            Wire::ThinkingStart { content_index } => {
                set_block(
                    blocks,
                    content_index,
                    AssistantContent::Thinking(ThinkingContent::default()),
                )?;
                Event::ThinkingStart {
                    content_index,
                    partial,
                }
            }
            Wire::ThinkingDelta {
                content_index,
                delta,
            } => {
                let Some(AssistantContent::Thinking(block)) = blocks.get_mut(content_index) else {
                    return Err(wrong_type("thinking_delta", "thinking"));
                };
                block.thinking.push_str(&delta);
                Event::ThinkingDelta {
                    content_index,
                    delta,
                    partial,
                }
            }
            Wire::ThinkingEnd {
                content_index,
                content_signature,
            } => {
                let Some(AssistantContent::Thinking(block)) = blocks.get_mut(content_index) else {
                    return Err(wrong_type("thinking_end", "thinking"));
                };
                block.thinking_signature = content_signature;
                Event::ThinkingEnd {
                    content_index,
                    content: block.thinking.clone(),
                    partial,
                }
            }
            Wire::ToolCallStart {
                content_index,
                id,
                tool_name,
            } => {
                set_block(
                    blocks,
                    content_index,
                    AssistantContent::ToolCall(ToolCall {
                        id,
                        name: tool_name,
                        ..ToolCall::default()
                    }),
                )?;
                self.tool_arguments
                    .insert(content_index, ProxyToolArguments::default());
                Event::ToolCallStart {
                    content_index,
                    partial,
                }
            }
            Wire::ToolCallDelta {
                content_index,
                delta,
            } => {
                let (Some(AssistantContent::ToolCall(block)), Some(args)) = (
                    blocks.get_mut(content_index),
                    self.tool_arguments.get_mut(&content_index),
                ) else {
                    return Err(wrong_type("toolcall_delta", "toolCall"));
                };
                // The parser returns the previous snapshot while accumulating enough input for
                // a new one. Do not copy a potentially large argument object on every delta.
                if let Some(snapshot) = args.parser.append(&delta)
                    && args
                        .displayed
                        .as_ref()
                        .is_none_or(|previous| !Arc::ptr_eq(previous, &snapshot))
                    && let Value::Object(arguments) = snapshot.as_ref()
                {
                    block.arguments.clone_from(arguments);
                    args.displayed = Some(snapshot);
                }
                Event::ToolCallDelta {
                    content_index,
                    delta,
                    partial,
                }
            }
            Wire::ToolCallEnd {
                content_index,
                tool_call,
            } => {
                let Some(AssistantContent::ToolCall(block)) = blocks.get_mut(content_index) else {
                    return Ok(false);
                };
                *block = tool_call.clone();
                self.tool_arguments.remove(&content_index);
                Event::ToolCallEnd {
                    content_index,
                    tool_call,
                    partial,
                }
            }
            Wire::Done {
                reason,
                usage,
                provider_thinking_level,
            } => {
                message.stop_reason = reason;
                message.usage = usage;
                message.provider_thinking_level = provider_thinking_level;
                return Ok(true);
            }
            Wire::Error {
                reason,
                usage,
                error_message,
                provider_thinking_level,
            } => {
                message.stop_reason = reason;
                message.usage = usage;
                message.error_message = error_message;
                message.provider_thinking_level = provider_thinking_level;
                return Ok(true);
            }
            Wire::Unknown => return Ok(false),
        };
        drop(message);
        tx.send(event);
        Ok(false)
    }
}

fn set_block(
    blocks: &mut Vec<AssistantContent>,
    index: usize,
    block: AssistantContent,
) -> pi_rs_ai::Result<()> {
    if let Some(existing) = blocks.get_mut(index) {
        *existing = block;
    } else if index == blocks.len() {
        blocks.push(block);
    } else {
        return Err(pi_rs_ai::Error::Protocol(format!(
            "Received proxy content index {index} before index {}",
            blocks.len()
        )));
    }
    Ok(())
}

fn wrong_type(event: &str, expected: &str) -> pi_rs_ai::Error {
    pi_rs_ai::Error::Protocol(format!("Received {event} for non-{expected} content"))
}

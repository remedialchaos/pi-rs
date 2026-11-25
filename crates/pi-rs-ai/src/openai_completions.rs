use std::collections::HashMap;

use reqwest::{Method, Response};
use serde_json::{Map, Value, json};

use crate::chat_state::ChatState;
use crate::constrained::{JsonGrammarInput, grammar, strict_parameters};
use crate::error::{Error, Result};
use crate::event_stream::{AssistantMessageEventStream, write_partial};
use crate::http::{HttpRequest, abortable, endpoint, payload, request_headers, send};
use crate::model::{CacheRetention, Model, ThinkingLevel};
use crate::openai_compat::{CompletionCompat, detect_compat, normalize_completion_id};
use crate::openrouter_images::image_data_url;
use crate::request::{ProviderHeaders, StreamOptions, provider_env_value};
use crate::simple_options::{
    build_base_options, clamp_thinking_budget_to_answer_room, resolve_sampling_params,
    thinking_budget_for_level,
};
use crate::sse::SseReader;
use crate::transcript::{
    TranscriptToolSupport, get_declared_tools, input_text, render_system_message_update,
    resolve_transcript, resolve_transcript_tools, system_message_text,
};
use crate::transform::transform_messages;
use crate::types::{
    AssistantContent, InputContent, Message, StopReason, StringOrBlocks, ThinkingContent, Tool,
    ToolCall, TranscriptContext, Usage,
};

pub(crate) fn retention(options: &StreamOptions) -> CacheRetention {
    options.cache_retention.unwrap_or_else(|| {
        if provider_env_value("PI_CACHE_RETENTION", &options.request.env).as_deref() == Some("long")
        {
            CacheRetention::Long
        } else {
            CacheRetention::Short
        }
    })
}
pub(crate) fn api_key<'o>(model: &Model, options: &'o StreamOptions) -> Result<&'o str> {
    if let Some(key) = options.request.api_key.as_deref().filter(|s| !s.is_empty()) {
        return Ok(key);
    }
    if options.request.headers.iter().any(|(name, v)| {
        v.is_some()
            && (name.eq_ignore_ascii_case("authorization")
                || name.eq_ignore_ascii_case("cf-aig-authorization"))
    }) {
        return Ok("unused");
    }
    Err(Error::Auth(format!(
        "No API key for provider: {}",
        model.provider
    )))
}
pub fn stream(
    model: Model,
    context: TranscriptContext,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    let (mut state, stream) = ChatState::new(&model);
    tokio::spawn(async move {
        let result = run(&mut state, &model, context, &options).await;
        state.finish(result);
    });
    stream
}
pub fn stream_simple(
    model: Model,
    context: TranscriptContext,
    options: StreamOptions,
) -> Result<AssistantMessageEventStream> {
    api_key(&model, &options)?;
    let options = build_base_options(&model, &context, options);
    Ok(stream(model, context, options))
}
fn grammar_map<'a>(
    context: &'a TranscriptContext,
    compat: &CompletionCompat<'_>,
) -> Result<HashMap<&'a str, &'a str>> {
    let mut fields = HashMap::new();
    for tool in get_declared_tools(context.messages()) {
        if let Some(g) = grammar(tool, compat.grammar)? {
            fields.insert(tool.name.as_str(), g.property);
        }
    }
    Ok(fields)
}
fn tool_wire(tool: &Tool, compat: &CompletionCompat<'_>) -> Result<Value> {
    if let Some(g) = grammar(tool, compat.grammar)? {
        return Ok(
            json!({"type":"custom","custom":{"name":tool.name,"description":tool.description,
            "format":{"type":"grammar","grammar":{"syntax":g.syntax,"definition":g.definition}}}}),
        );
    }
    let (params, strict) = strict_parameters(tool, compat.strict, |_, _| false)?;
    let mut body =
        json!({"name":tool.name,"description":tool.description,"parameters":params.as_ref()});
    if compat.strict {
        body["strict"] = Value::Bool(strict.unwrap_or(false));
    }
    Ok(json!({"type":"function","function":body}))
}
fn to_input(block: InputContent) -> Option<Value> {
    match block {
        InputContent::Text(t) if !t.text.is_empty() => Some(json!({"type":"text","text":t.text})),
        InputContent::Image(i) => {
            Some(json!({"type":"image_url","image_url":{"url":image_data_url(&i)}}))
        }
        _ => None,
    }
}
fn reasoning_field(field: &str) -> bool {
    matches!(field, "reasoning_content" | "reasoning" | "reasoning_text")
}
fn valid_detail(detail: &Value) -> bool {
    if !detail.is_object()
        || detail
            .get("id")
            .is_some_and(|v| !v.is_null() && !v.is_string())
        || detail.get("format").is_some_and(|v| !v.is_string())
        || detail.get("index").is_some_and(|v| !v.is_number())
    {
        return false;
    }
    match detail["type"].as_str() {
        Some("reasoning.summary") => detail["summary"].is_string(),
        Some("reasoning.encrypted") => detail["data"].is_string(),
        Some("reasoning.text") => {
            detail["text"].is_string()
                && detail
                    .get("signature")
                    .is_none_or(|v| v.is_null() || v.is_string())
        }
        _ => false,
    }
}
fn reasoning_details(a: &crate::types::AssistantMessage) -> Option<Value> {
    let details = a.content.iter().find_map(|block| {
        let AssistantContent::Thinking(block) = block else {
            return None;
        };
        let p = serde_json::from_str::<Value>(block.thinking_signature.as_deref()?).ok()?;
        p.as_array()
            .filter(|a| !a.is_empty() && a.iter().all(valid_detail))?;
        Some(p)
    });
    if details.is_some() {
        return details;
    }
    let legacy = a
        .content
        .iter()
        .filter_map(|block| {
            let AssistantContent::ToolCall(block) = block else {
                return None;
            };
            let p = serde_json::from_str::<Value>(block.thought_signature.as_deref()?).ok()?;
            (valid_detail(&p)
                && p["type"] == "reasoning.encrypted"
                && p["id"].as_str().is_some_and(|s| !s.is_empty())
                && p["data"].as_str().is_some_and(|s| !s.is_empty()))
            .then_some(p)
        })
        .collect::<Vec<_>>();
    (!legacy.is_empty()).then_some(Value::Array(legacy))
}
fn assistant_wire(
    a: crate::types::AssistantMessage,
    model: &Model,
    compat: &CompletionCompat<'_>,
    fields: &HashMap<String, String>,
) -> Result<Option<Value>> {
    let details = reasoning_details(&a);
    let mut text = String::new();
    let mut raw_thinking = String::new();
    let mut calls = Vec::new();
    let mut signature = None;
    let mut text_parts = Vec::new();
    for block in a.content {
        match block {
            AssistantContent::ToolCall(t) => {
                if let Some(property) = fields.get(&t.name) {
                    let input=t.arguments.get(property).and_then(Value::as_str).ok_or_else(||Error::Protocol(format!("Grammar tool call \"{}\" requires argument \"{property}\" to be a string.",t.name)))?;
                    calls.push(
                        json!({"id":t.id,"type":"custom","custom":{"name":t.name,"input":input}}),
                    );
                } else {
                    calls.push(json!({"id":t.id,"type":"function","function":{"name":t.name,"arguments":serde_json::to_string(&t.arguments)?}}));
                }
            }
            AssistantContent::Text(t) if !t.text.trim().is_empty() => {
                if compat.thinking_as_text {
                    text_parts.push(json!({"type":"text","text":t.text}));
                } else {
                    text.push_str(&t.text);
                }
            }
            AssistantContent::Thinking(t) if !t.thinking.trim().is_empty() => {
                if raw_thinking.is_empty() {
                    signature = t.thinking_signature;
                } else {
                    raw_thinking.push_str(if compat.thinking_as_text {
                        "\n\n"
                    } else {
                        "\n"
                    });
                }
                raw_thinking.push_str(&t.thinking);
            }
            _ => {}
        }
    }
    let mut response = json!({"role":"assistant","content":if compat.assistant_after_tool||compat.non_null_assistant{Value::String(String::new())}else{Value::Null}});
    if compat.thinking_as_text {
        if !raw_thinking.is_empty() {
            text_parts.insert(0, json!({"type":"text","text":raw_thinking}));
            response["content"] = Value::Array(text_parts);
        } else if !text_parts.is_empty() {
            for t in text_parts {
                if let Some(part) = t["text"].as_str() {
                    text.push_str(part);
                }
            }
            response["content"] = Value::String(text);
        }
    } else {
        if !text.is_empty() {
            response["content"] = Value::String(text);
        }
        if details.is_none()
            && !raw_thinking.is_empty()
            && let Some(mut sig) = signature.filter(|s| reasoning_field(s))
        {
            if model.provider == "opencode-go" && sig == "reasoning" {
                sig = "reasoning_content".into();
            }
            response[sig] = Value::String(raw_thinking);
        }
    }
    if let Some(details) = details {
        response["reasoning_details"] = details;
    }
    if compat.assistant_reasoning_content
        && model.reasoning
        && response.get("reasoning_content").is_none()
    {
        response["reasoning_content"] = Value::String(String::new());
    }
    if calls.is_empty()
        && response["content"].as_str().is_none_or(str::is_empty)
        && !response["content"].is_array()
    {
        return Ok(None);
    }
    if !calls.is_empty() {
        response["tool_calls"] = Value::Array(calls);
    }
    Ok(Some(response))
}

fn convert(
    model: &Model,
    context: TranscriptContext,
    compat: &CompletionCompat<'_>,
    fields: &HashMap<String, String>,
    folded: usize,
) -> Result<Vec<Value>> {
    let messages = transform_messages(context.into_messages(), model, |id, m, _| {
        normalize_completion_id(id, m)
    });
    let mut out = Vec::with_capacity(messages.len());
    let mut system_index = 0;
    let mut last_tool = false;
    let mut messages = messages.into_iter().enumerate().peekable();
    while let Some((i, m)) = messages.next() {
        if last_tool && compat.assistant_after_tool && matches!(m, Message::User(_)) {
            out.push(json!({"role":"assistant","content":"I have processed the tool results."}));
        }
        last_tool = false;
        match m {
            Message::System(system) => {
                if system_index >= folded
                    && let Some(tools) = system.tools_added.as_ref().filter(|t| !t.is_empty())
                {
                    let tools = tools
                        .iter()
                        .map(|t| tool_wire(t, compat))
                        .collect::<Result<Vec<_>>>()?;
                    out.push(json!({"role":"system","tools":tools}));
                }
                system_index += 1;
                let text = if i == 0 {
                    system_message_text(&system)
                } else {
                    render_system_message_update(&system)
                };
                if !text.is_empty() {
                    out.push(json!({"role":if model.reasoning&&compat.developer{"developer"}else{"system"},"content":text}));
                }
            }
            Message::User(user) => match user.content {
                StringOrBlocks::String(s) => out.push(json!({"role":"user","content":s})),
                StringOrBlocks::Blocks(parts) => {
                    let parts = parts.into_iter().filter_map(to_input).collect::<Vec<_>>();
                    if !parts.is_empty() {
                        out.push(json!({"role":"user","content":parts}));
                    }
                }
            },
            Message::Assistant(a) => {
                if let Some(part) = assistant_wire(a, model, compat, fields)? {
                    out.push(part);
                }
            }
            Message::ToolResult(mut t) => {
                let mut images = Vec::new();
                loop {
                    let text = input_text(&t.content, "\n");
                    let has_images = t
                        .content
                        .iter()
                        .any(|p| matches!(p, InputContent::Image(_)));
                    let content = if !text.trim().is_empty() {
                        text.as_str()
                    } else if has_images {
                        "(see attached image)"
                    } else {
                        "(no tool output)"
                    };
                    let mut result =
                        json!({"role":"tool","content":content,"tool_call_id":t.tool_call_id});
                    if compat.tool_result_name && !t.tool_name.is_empty() {
                        result["name"] = Value::String(t.tool_name);
                    }
                    out.push(result);
                    if model.supports_images() {
                        images.extend(
                            t.content
                                .into_iter()
                                .filter(|p| matches!(p, InputContent::Image(_)))
                                .filter_map(to_input),
                        );
                    }
                    if matches!(messages.peek(), Some((_, Message::ToolResult(_))))
                        && let Some((_, Message::ToolResult(next))) = messages.next()
                    {
                        t = next;
                    } else {
                        break;
                    }
                }
                if images.is_empty() {
                    last_tool = true;
                } else {
                    if compat.assistant_after_tool {
                        out.push(json!({"role":"assistant","content":"I have processed the tool results."}));
                    }
                    images.insert(
                        0,
                        json!({"type":"text","text":"Attached image(s) from tool result:"}),
                    );
                    out.push(json!({"role":"user","content":images}));
                }
            }
        }
    }
    Ok(out)
}
fn cache_control(msg: &mut Value, control: &Value) -> bool {
    if let Some(text) = msg
        .get("content")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        msg["content"] = json!([{"type":"text","text":text,"cache_control":control}]);
        return true;
    }
    if let Some(text) = msg["content"]
        .as_array_mut()
        .and_then(|parts| parts.iter_mut().rev().find(|p| p["type"] == "text"))
    {
        text["cache_control"] = control.clone();
        return true;
    }
    false
}
fn add_reasoning(
    body: &mut Value,
    model: &Model,
    options: &StreamOptions,
    c: &CompletionCompat<'_>,
) {
    if !model.reasoning {
        return;
    }
    let level = options
        .reasoning
        .map(|r| model.clamp_thinking_level(r))
        .filter(|r| *r != ThinkingLevel::Off);
    let override_effort = options
        .specific
        .get("reasoningEffort")
        .and_then(Value::as_str);
    let mapped_level = level.unwrap_or(ThinkingLevel::Off);
    let mapped = model
        .thinking_level_map
        .as_ref()
        .and_then(|m| m.get(&mapped_level));
    let effort = override_effort.or_else(|| {
        mapped
            .and_then(Option::as_deref)
            .or_else(|| level.map(ThinkingLevel::as_str))
    });
    let enabled = level.is_some() || override_effort.is_some();
    let allow_off = mapped != Some(&None);
    let budget = level
        .map(|level| {
            clamp_thinking_budget_to_answer_room(
                thinking_budget_for_level(level, &options.thinking_budgets),
                options.max_tokens.unwrap_or(model.max_tokens),
            )
        })
        .filter(|b| *b > 0);
    let field = |body: &mut Value, e: Option<&str>| {
        if let Some(e) = e {
            body["reasoning_effort"] = json!(e);
        }
    };
    match c.thinking_format {
        "zai" => {
            body["thinking"] = if enabled {
                json!({"type":"enabled","clear_thinking":false})
            } else {
                json!({"type":"disabled"})
            };
            if enabled && c.reasoning_effort {
                field(body, effort);
            }
        }
        "qwen" => {
            body["enable_thinking"] = json!(enabled);
            if enabled && c.reasoning_effort {
                field(body, effort);
            }
        }
        "qwen-chat-template" => {
            body["chat_template_kwargs"] =
                json!({"enable_thinking":enabled,"preserve_thinking":true})
        }
        "chat-template" | "baseten" => {
            let (key, templates) = if c.thinking_format == "baseten" {
                ("chat_template_args", c.chat_template_args)
            } else {
                ("chat_template_kwargs", c.chat_template_kwargs)
            };
            if let Some(templates) = templates {
                let fields: Map<_, _> = templates
                    .iter()
                    .filter_map(|(k, v)| {
                        let v = if v.is_object() {
                            if !enabled && v["omitWhenOff"].as_bool() == Some(true) {
                                return None;
                            }
                            match v["$var"].as_str() {
                                Some("thinking.enabled") => json!(enabled),
                                Some("thinking.budget") => json!(budget?),
                                _ => json!(effort?),
                            }
                        } else {
                            v.clone()
                        };
                        Some((k.clone(), v))
                    })
                    .collect();
                if !fields.is_empty() {
                    body[key] = Value::Object(fields);
                }
            }
            if c.thinking_format == "baseten" && c.reasoning_effort && allow_off {
                field(body, effort);
            }
        }
        "deepseek" => {
            if enabled || allow_off {
                body["thinking"] = json!({"type":if enabled{"enabled"}else{"disabled"}});
            }
            if enabled && c.reasoning_effort {
                field(body, effort);
            }
        }
        "openrouter" => {
            if enabled || allow_off {
                body["reasoning"] = json!({"effort":effort.unwrap_or("none")});
            }
        }
        "ant-ling" => {
            if enabled && let Some(e) = mapped.and_then(Option::as_deref) {
                body["reasoning"] = json!({"effort":e});
            }
        }
        "together" => {
            body["reasoning"] = json!({"enabled":enabled});
            if enabled && c.reasoning_effort {
                field(body, effort);
            }
        }
        "string-thinking" => {
            if enabled || allow_off {
                body["thinking"] = json!(effort.unwrap_or("none"));
            }
        }
        _ => {
            if c.reasoning_effort {
                field(body, effort);
            }
        }
    }
    if let (Some(k), Some(budget)) = (c.thinking_budget_field, budget) {
        body[k] = json!(budget);
    }
}
fn build_body(
    model: &Model,
    context: TranscriptContext,
    options: &StreamOptions,
    c: &CompletionCompat<'_>,
) -> Result<(Value, HashMap<String, String>)> {
    let context = resolve_transcript(context, c.mid_conversation_system);
    let tool_support = if c.mid_conversation_system && c.mid_conversation_tools {
        TranscriptToolSupport::Additions
    } else {
        TranscriptToolSupport::None
    };
    let t = resolve_transcript_tools(context.messages(), tool_support);
    let fields: HashMap<String, String> = grammar_map(&context, c)?
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let mut tools = t
        .request_tools
        .iter()
        .map(|t| tool_wire(t, c))
        .collect::<Result<Vec<_>>>()?;
    let history=context.messages().iter().any(|m|matches!(m,Message::ToolResult(_))||matches!(m,Message::Assistant(a) if a.content.iter().any(|p|matches!(p,AssistantContent::ToolCall(_)))));
    let folded = t.folded_system_messages;
    let mut messages = convert(model, context, c, &fields, folded)?;
    let cache = retention(options);
    if c.anthropic_cache && cache != CacheRetention::None {
        let cc = if cache == CacheRetention::Long && c.long_cache {
            json!({"type":"ephemeral","ttl":"1h"})
        } else {
            json!({"type":"ephemeral"})
        };
        if let Some(system) = messages
            .iter_mut()
            .find(|m| matches!(m["role"].as_str(), Some("system" | "developer")))
        {
            cache_control(system, &cc);
        }
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = cc.clone();
        }
        for m in messages
            .iter_mut()
            .rev()
            .filter(|m| matches!(m["role"].as_str(), Some("user" | "assistant" | "tool")))
        {
            if cache_control(m, &cc) {
                break;
            }
        }
    }
    let mut body = json!({"model":model.id,"messages":messages,"stream":true});
    let supports_prompt_key = c.prompt_key_mode == "enabled"
        || c.prompt_key_mode == "auto"
            && (model.base_url.contains("api.openai.com")
                || cache == CacheRetention::Long && c.long_cache);
    if cache != CacheRetention::None
        && supports_prompt_key
        && let Some(key) = &options.session_id
    {
        body["prompt_cache_key"] = json!(key.chars().take(64).collect::<String>());
    }
    if cache == CacheRetention::Long && c.long_cache {
        body["prompt_cache_retention"] = json!("24h");
    }
    if c.usage {
        body["stream_options"] = json!({"include_usage":true});
    }
    if c.supports_store {
        body["store"] = Value::Bool(false);
    }
    if let Some(max) = options.max_tokens.filter(|n| *n > 0) {
        body[c.max_tokens_field] = json!(max);
    }
    if let Some(temp) = options.temperature {
        body["temperature"] = json!(temp);
    }
    if !tools.is_empty() {
        if c.zai_tool_stream {
            body["tool_stream"] = json!(true);
        }
        body["tools"] = Value::Array(tools);
    } else if c.tools_with_history && history {
        body["tools"] = json!([]);
    }
    if let Some(choice) = options.specific.get("toolChoice") {
        body["tool_choice"] = choice.clone();
    } else if let Some(choice) = options.tool_choice {
        body["tool_choice"] = json!(choice);
    }
    if let Some(priority) = c.vllm_priority {
        body["priority"] = priority.clone();
    }
    if let Some(routing) = c.openrouter_routing {
        body["provider"] = routing.clone();
    }
    if let Some(routing) = c.vercel_routing.and_then(Value::as_object) {
        let gateway = routing
            .iter()
            .filter(|(key, _)| matches!(key.as_str(), "only" | "order"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Map<_, _>>();
        if !gateway.is_empty() {
            body["providerOptions"] = json!({"gateway":gateway});
        }
    }
    add_reasoning(&mut body, model, options, c);
    if let Value::Object(params) = &mut body {
        params.extend(resolve_sampling_params(
            model,
            options.reasoning.unwrap_or(ThinkingLevel::Off),
            options.sampling_params.clone(),
        ));
    }
    Ok((body, fields))
}

pub(crate) fn copilot_headers(
    model: &Model,
    context: &TranscriptContext,
    headers: &mut ProviderHeaders,
) {
    if model.provider != "github-copilot" {
        return;
    }
    let initiated = if context
        .messages()
        .last()
        .is_none_or(|m| matches!(m, Message::User(_)))
    {
        "user"
    } else {
        "agent"
    };
    headers.insert("X-Initiator".into(), Some(initiated.into()));
    headers.insert("Openai-Intent".into(), Some("conversation-edits".into()));
    if context.messages().iter().any(|m| {
        let parts = match m {
            Message::User(u) => match &u.content {
                StringOrBlocks::Blocks(parts) => parts.as_slice(),
                _ => &[],
            },
            Message::ToolResult(r) => r.content.as_slice(),
            _ => &[],
        };
        parts.iter().any(|p| matches!(p, InputContent::Image(_)))
    }) {
        headers.insert("Copilot-Vision-Request".into(), Some("true".into()));
    }
}
pub(crate) fn add_affinity(headers: &mut ProviderHeaders, format: &str, session: &str) {
    if format == "openrouter" {
        headers.insert("x-session-id".into(), Some(session.into()));
    } else {
        if format == "openai" {
            headers.insert("session_id".into(), Some(session.into()));
        }
        headers.insert("x-client-request-id".into(), Some(session.into()));
        headers.insert("x-session-affinity".into(), Some(session.into()));
    }
}
fn usage(raw: &Value, model: &Model) -> Usage {
    let get = |k| raw[k].as_f64().unwrap_or(0.0);
    let details = &raw["prompt_tokens_details"];
    let read = details["cached_tokens"]
        .as_f64()
        .or_else(|| raw["prompt_cache_hit_tokens"].as_f64())
        .unwrap_or_else(|| get("cached_tokens"));
    let write = details["cache_write_tokens"].as_f64().unwrap_or(0.0);
    let input = (get("prompt_tokens") - read - write).max(0.0);
    let output = get("completion_tokens");
    let reasoning = raw["completion_tokens_details"]["reasoning_tokens"]
        .as_f64()
        .filter(|n| *n != 0.0)
        .unwrap_or_else(|| get("reasoning_tokens"));
    let mut usage = Usage {
        input,
        output,
        cache_read: read,
        cache_write: write,
        reasoning: Some(reasoning),
        total_tokens: input + output + read + write,
        ..Usage::default()
    };
    usage.calculate_cost(&model.cost);
    if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
        apply_openrouter_cost(&mut usage, raw);
    }
    usage
}
pub fn apply_openrouter_cost(usage: &mut Usage, raw: &Value) {
    let valid = |v: &Value| v.as_f64().filter(|n| n.is_finite() && *n >= 0.0);
    let Some(cost) = valid(&raw["cost"]) else {
        return;
    };
    let actual = match raw["is_byok"].as_bool() {
        Some(false) => cost,
        Some(true) => {
            let detail = &raw["cost_details"];
            let upstream = if detail.get("upstream_inference_cost") == Some(&Value::Null) {
                valid(&detail["upstream_inference_prompt_cost"])
                    .zip(valid(&detail["upstream_inference_completions_cost"]))
                    .map(|(a, b)| a + b)
                    .filter(|n| n.is_finite())
            } else {
                valid(&detail["upstream_inference_cost"])
            };
            let Some(upstream) = upstream else { return };
            cost + upstream
        }
        None => return,
    };
    usage.cost.total = actual;
    usage.cost.total_from_provider = Some(true);
}
fn stop_reason(raw: &str) -> Result<StopReason> {
    match raw {
        "stop" | "end" => Ok(StopReason::Stop),
        "length" => Ok(StopReason::Length),
        "function_call" | "tool_calls" => Ok(StopReason::ToolUse),
        _ => Err(Error::Protocol(format!("Provider finish_reason: {raw}"))),
    }
}
struct Custom {
    field: String,
    input: JsonGrammarInput,
}
async fn run(
    state: &mut ChatState,
    model: &Model,
    context: TranscriptContext,
    options: &StreamOptions,
) -> Result<StopReason> {
    let key = api_key(model, options)?;
    let c = detect_compat(model);
    let mut headers = ProviderHeaders::from([
        ("authorization".into(), Some(format!("Bearer {key}"))),
        ("content-type".into(), Some("application/json".into())),
        ("accept".into(), Some("text/event-stream".into())),
    ]);
    copilot_headers(model, &context, &mut headers);
    if c.send_affinity
        && retention(options) != CacheRetention::None
        && let Some(session) = options.session_id.as_deref().filter(|s| !s.is_empty())
    {
        add_affinity(&mut headers, c.affinity_format, session);
    }
    let (body, fields) = build_body(model, context, options, &c)?;
    let body = payload(body, model, &options.request).await?;
    let request = HttpRequest {
        method: Method::POST,
        url: endpoint(&model.base_url, "chat/completions")?,
        headers: request_headers(&model.base, &options.request, headers)?,
        body: Some(serde_json::to_vec(&body)?),
        error_label: "OpenAI Completions API error".into(),
        force_http1: false,
        default_retries: 0,
        no_retry_statuses: Vec::new(),
    };
    let response = send(request, model, &options.request).await?;
    consume(response, state, model, options, c.finish_reason, &fields).await
}
async fn consume(
    response: Response,
    state: &mut ChatState,
    model: &Model,
    options: &StreamOptions,
    require_finish: bool,
    fields: &HashMap<String, String>,
) -> Result<StopReason> {
    state.start();
    let mut sse = SseReader::new(response.bytes_stream());
    let mut text_index = None;
    let mut thinking_index = None;
    let mut details = Vec::new();
    let mut finish = None;
    let mut by_index = HashMap::new();
    let mut by_id = HashMap::new();
    let mut custom = HashMap::<usize, Custom>::new();
    while let Some(frame) = sse.next(&options.request.signal).await? {
        if frame.data.trim() == "[DONE]" {
            break;
        }
        if frame.data.trim().is_empty() {
            continue;
        }
        let chunk: Value = serde_json::from_str(&frame.data)?;
        if let Some(watch) = &options.on_provider_stream_event {
            abortable(&options.request.signal, watch(&chunk, model)).await?;
        }
        if let Some(error) = chunk.get("error").filter(|v| !v.is_null()) {
            return Err(Error::Protocol(
                error["message"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| error.to_string()),
            ));
        }
        let choice = &chunk["choices"][0];
        {
            let mut msg = write_partial(&state.message);
            if msg.response_id.is_none() {
                msg.response_id = chunk["id"].as_str().map(str::to_owned);
            }
            if msg.response_model.is_none() {
                msg.response_model = chunk["model"]
                    .as_str()
                    .filter(|id| !id.is_empty() && *id != model.id)
                    .map(str::to_owned);
            }
            let raw = chunk
                .get("usage")
                .filter(|v| !v.is_null())
                .or_else(|| choice.get("usage").filter(|v| !v.is_null()));
            if let Some(raw) = raw {
                msg.usage = usage(raw, model);
            }
            if let Some(reason) = choice["finish_reason"].as_str().filter(|s| !s.is_empty()) {
                msg.raw_stop_reason = Some(reason.to_owned());
                finish = Some(stop_reason(reason));
            }
        }
        let delta = &choice["delta"];
        if let Some((field, content)) = ["reasoning_content", "reasoning", "reasoning_text"]
            .into_iter()
            .find_map(|name| {
                delta[name]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|s| (name, s))
            })
        {
            let index = *thinking_index.get_or_insert_with(|| {
                state.thinking_start(ThinkingContent {
                    thinking_signature: Some(
                        if model.provider == "opencode-go" && field == "reasoning" {
                            "reasoning_content".into()
                        } else {
                            field.into()
                        },
                    ),
                    ..ThinkingContent::default()
                })
            });
            state.delta(index, content.to_owned())?;
        }
        if let Some(parts) = delta["reasoning_details"].as_array() {
            for part in parts.iter().filter(|p| valid_detail(p)) {
                thinking_index
                    .get_or_insert_with(|| state.thinking_start(ThinkingContent::default()));
                append_detail(&mut details, part);
            }
        }
        if let Some(content) = delta["content"].as_str().filter(|s| !s.is_empty()) {
            let index = *text_index.get_or_insert_with(|| state.text_start(None));
            state.delta(index, content.to_owned())?;
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let provider_index = call["index"].as_u64();
            let provider_id = call["id"].as_str().filter(|s| !s.is_empty());
            let name = call["function"]["name"]
                .as_str()
                .or_else(|| call["custom"]["name"].as_str())
                .unwrap_or_default();
            let is_custom = call["custom"].is_object() && !call["function"].is_object();
            let index = provider_index
                .and_then(|i| by_index.get(&i))
                .or_else(|| provider_id.and_then(|id| by_id.get(id)))
                .copied()
                .unwrap_or_else(|| {
                    let mut call = ToolCall {
                        id: provider_id.unwrap_or_default().into(),
                        name: name.into(),
                        ..ToolCall::default()
                    };
                    let field = is_custom.then(|| {
                        fields
                            .get(name)
                            .map(String::as_str)
                            .unwrap_or("input")
                            .to_owned()
                    });
                    if let Some(field) = &field {
                        call.arguments
                            .insert(field.clone(), Value::String(String::new()));
                    }
                    let index = state.tool_start(call);
                    if let Some(field) = field {
                        custom.insert(
                            index,
                            Custom {
                                field,
                                input: Default::default(),
                            },
                        );
                    }
                    index
                });
            if let Some(i) = provider_index {
                by_index.insert(i, index);
            }
            if let Some(id) = provider_id {
                by_id.insert(id.to_owned(), index);
            }
            state.tool_update(index, |t| {
                if t.id.is_empty()
                    && let Some(id) = provider_id
                {
                    t.id = id.into();
                }
                if t.name.is_empty() {
                    t.name = name.into();
                }
            });
            if is_custom {
                let info = custom.entry(index).or_insert_with(|| Custom {
                    field: fields.get(name).cloned().unwrap_or_else(|| "input".into()),
                    input: Default::default(),
                });
                if let Some(raw) = call["custom"]["input"].as_str() {
                    state.delta(index, info.input.delta(&info.field, raw)?)?;
                }
            } else if let Some(args) = call["function"]["arguments"].as_str() {
                state.delta(index, args.to_owned())?;
            }
        }
    }
    if let Some(index) = thinking_index.filter(|_| !details.is_empty()) {
        let details = serde_json::to_string(&details)?;
        state.thinking_update(index, |t| t.thinking_signature = Some(details));
    }
    for (index, mut x) in custom {
        state.delta(index, x.input.close(&x.field, None)?)?;
    }
    if let Some(result) = finish {
        return result;
    }
    if require_finish {
        Err(Error::Protocol("Stream ended without finish_reason".into()))
    } else {
        Ok(if state.has_tool_calls() {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        })
    }
}
fn append_detail(out: &mut Vec<Value>, part: &Value) {
    let field = match part["type"].as_str() {
        Some("reasoning.text") => "text",
        Some("reasoning.summary") => "summary",
        _ => {
            out.push(part.clone());
            return;
        }
    };
    if let Some(last) = out.last_mut().filter(|v| v["type"] == part["type"]) {
        let mut text = last[field].as_str().unwrap_or_default().to_owned();
        text.push_str(part[field].as_str().unwrap_or_default());
        last[field] = Value::String(text);
        for key in ["id", "format", "index", "signature"] {
            if (last[key].is_null() || last[key].as_str() == Some(""))
                && let Some(v) = part.get(key)
            {
                last[key] = v.clone();
            }
        }
    } else {
        out.push(part.clone());
    }
}

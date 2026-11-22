use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use indexmap::{IndexMap, IndexSet};
use reqwest::{Method, Response, StatusCode};
use serde_json::{Value, json};

use crate::chat_state::ChatState;
use crate::constrained::strict_parameters;
use crate::error::{Error, Result};
use crate::event_stream::{AssistantMessageEventStream, read_partial, write_partial};
use crate::http::{HttpRequest, abortable, endpoint, payload, request_headers, send};
use crate::model::{Api, CacheRetention, Model, ModelCost, ThinkingLevel};
use crate::openai_compat::short_hash;
use crate::openai_completions::{apply_openrouter_cost, copilot_headers, retention};
use crate::request::{ProviderHeaders, StreamOptions, provider_env_value};
use crate::simple_options::{
    adjust_max_tokens_for_thinking, build_base_options, clamp_max_tokens_to_context,
};
use crate::sse::SseReader;
use crate::transcript::{
    get_current_tools, get_initial_system_message, input_text, render_system_message_update,
    resolve_transcript, system_message_text,
};
use crate::transform::{split_error_tool_result_images, transform_messages};
use crate::types::{
    AssistantContent, ImageContent, InputContent, Message, StopReason, StringOrBlocks,
    ThinkingContent, Tool, ToolCall, TranscriptContext, Usage, unix_millis,
};

const THREAD_BETA: &str = "message-threads-2026-08-12";
const MID_EFFORT: &str = "mid-conversation-output-config-2026-07-01";
const THINKING_BINDING: &str = "thinking-binding-controls-2026-08-01";
const INLINE_TOOLS: &str = "inline-tools-2026-09-15";
const INTERLEAVED: &str = "interleaved-thinking-2025-05-14";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const EAGER_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
const MAX_CACHE: usize = 10_000;

struct ThreadEntry {
    count: usize,
    hash: String,
    id: String,
}
static THREADS: LazyLock<Mutex<IndexMap<String, Option<ThreadEntry>>>> =
    LazyLock::new(|| Mutex::new(IndexMap::new()));
static OVER_LIMIT: LazyLock<Mutex<IndexSet<String>>> =
    LazyLock::new(|| Mutex::new(IndexSet::new()));

pub fn close_sessions(session: Option<&str>) {
    let mut contexts = THREADS.lock().unwrap_or_else(|e| e.into_inner());
    let mut strict = OVER_LIMIT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = session {
        contexts.shift_remove(s);
        strict.shift_remove(s);
    } else {
        contexts.clear();
        strict.clear();
    }
}
pub(crate) fn strict_limit_error(message: &str) -> bool {
    [
        "The compiled grammar is too large",
        "Too many strict tools (",
        "Schemas contains too many parameters with union types (",
    ]
    .iter()
    .any(|phrase| message.contains(phrase))
}
pub(crate) fn mark_over_limit(session: Option<&str>) {
    if let Some(session) = session {
        let mut cache = OVER_LIMIT.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(session.to_owned());
        if cache.len() > MAX_CACHE {
            cache.shift_remove_index(0);
        }
    }
}
pub(crate) fn exceeded_strict(session: Option<&str>) -> bool {
    session.is_some_and(|s| {
        OVER_LIMIT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(s)
    })
}

fn flag(model: &Model, key: &str, default: bool) -> bool {
    model.compat(key).as_bool().unwrap_or(default)
}
fn oauth_key(key: &str) -> bool {
    key.contains("sk-ant-oat")
}
fn auth_header(options: &StreamOptions) -> bool {
    options.request.headers.iter().any(|(k, v)| {
        v.as_deref().is_some_and(|s| !s.trim().is_empty())
            && ["authorization", "x-api-key", "cf-aig-authorization"]
                .iter()
                .any(|h| k.eq_ignore_ascii_case(h))
    })
}
fn oauth_name(name: &str) -> &str {
    const CLAUDE: &[&str] = &[
        "Read",
        "Write",
        "Edit",
        "Bash",
        "Grep",
        "Glob",
        "AskUserQuestion",
        "EnterPlanMode",
        "ExitPlanMode",
        "KillShell",
        "NotebookEdit",
        "Skill",
        "Task",
        "TaskOutput",
        "TodoWrite",
        "WebFetch",
        "WebSearch",
    ];
    CLAUDE
        .iter()
        .copied()
        .find(|s| s.eq_ignore_ascii_case(name))
        .unwrap_or(name)
}
fn oauth_tool(name: &str, oauth: bool) -> &str {
    if oauth { oauth_name(name) } else { name }
}
fn auth_available(model: &Model, opts: &StreamOptions) -> bool {
    opts.request
        .api_key
        .as_deref()
        .is_some_and(|s| !s.is_empty())
        || auth_header(opts)
        || federation_config(model, opts).is_some()
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
    if !auth_available(&model, &opts) {
        return Err(Error::Auth(format!(
            "No API key for provider: {}",
            model.provider
        )));
    }
    let mut opts = build_base_options(&model, &context, opts);
    let level = opts.reasoning;
    opts.specific
        .insert("thinkingEnabled".into(), json!(level.is_some()));
    if let Some(level) = level {
        if flag(&model, "forceAdaptiveThinking", false) {
            let fallback = match level {
                ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
                ThinkingLevel::Medium => "medium",
                _ => "high",
            };
            let effort = model
                .thinking_level_map
                .as_ref()
                .and_then(|m| m.get(&level))
                .and_then(Option::as_deref)
                .unwrap_or(fallback);
            opts.specific.insert("effort".into(), json!(effort));
        } else {
            let (max, budget) = adjust_max_tokens_for_thinking(
                opts.max_tokens,
                model.max_tokens,
                level,
                &opts.thinking_budgets,
            );
            let max = clamp_max_tokens_to_context(&model, &context, max);
            opts.max_tokens = Some(max);
            opts.specific.insert(
                "thinkingBudgetTokens".into(),
                json!(budget.min(max.saturating_sub(1024))),
            );
        }
    }
    Ok(stream(model, context, opts))
}

fn normalize(id: &str) -> String {
    id.encode_utf16()
        .take(64)
        .map(|c| {
            let c = char::from_u32(c as u32).unwrap_or('_');
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}
fn part(block: &InputContent) -> Value {
    match block {
        InputContent::Text(t) => json!({"type":"text","text":t.text}),
        InputContent::Image(i) => image(i),
    }
}
fn image(i: &ImageContent) -> Value {
    json!({"type":"image","source":{"type":"base64","media_type":i.mime_type,"data":i.data}})
}
fn tool_result_content(content: &[InputContent]) -> Value {
    if !content.iter().any(|c| matches!(c, InputContent::Image(_))) {
        return Value::String(input_text(content, "\n"));
    }
    let mut blocks: Vec<_> = content.iter().map(part).collect();
    if !content.iter().any(|c| matches!(c, InputContent::Text(_))) {
        blocks.insert(0, json!({"type":"text","text":"(see attached image)"}));
    }
    Value::Array(blocks)
}
fn assistant_blocks(
    message: &crate::types::AssistantMessage,
    oauth: bool,
    empty: bool,
) -> Vec<Value> {
    message.content.iter().filter_map(|block|Some(match block{
        AssistantContent::Text(t) if !t.text.trim().is_empty()=>json!({"type":"text","text":t.text}),
        AssistantContent::Thinking(t) if t.redacted==Some(true)=>json!({"type":"redacted_thinking","data":t.thinking_signature}),
        AssistantContent::Thinking(t)=>{
            let signature=t.thinking_signature.as_deref().filter(|s|!s.trim().is_empty());
            if t.thinking.trim().is_empty()&&signature.is_none(){return None;}
            if signature.is_some()||empty{json!({"type":"thinking","thinking":t.thinking,"signature":signature.unwrap_or_default()})}
            else {json!({"type":"text","text":t.thinking})}
        }
        AssistantContent::ToolCall(t)=>json!({"type":"tool_use","id":t.id,"name":oauth_tool(&t.name,oauth),"input":t.arguments}),
        _=>return None,
    })).collect()
}
fn no_strict_keyword(key: &str, value: &Value) -> bool {
    matches!(
        key,
        "minimum"
            | "maximum"
            | "exclusiveMinimum"
            | "exclusiveMaximum"
            | "multipleOf"
            | "maxItems"
            | "uniqueItems"
            | "minContains"
            | "maxContains"
            | "minProperties"
            | "maxProperties"
    ) || key == "minItems" && !(value == 0 || value == 1)
        || key == "format"
            && !matches!(
                value.as_str(),
                Some(
                    "date-time"
                        | "time"
                        | "date"
                        | "duration"
                        | "email"
                        | "hostname"
                        | "uri"
                        | "ipv4"
                        | "ipv6"
                        | "uuid"
                )
            )
}
fn prefers(tool: &Tool) -> bool {
    use crate::types::{
        ConstrainedSampling as C, ConstrainedSamplingConfig as CC, StrictPreference,
    };
    matches!(
        tool.constrained_sampling,
        Some(C::Enabled(CC::JsonSchema {
            strict: StrictPreference::Prefer
        }))
    )
}
struct Builder<'a> {
    model: &'a Model,
    oauth: bool,
    disable_preferred: bool,
    saw_preferred: bool,
}
impl Builder<'_> {
    fn tool(&mut self, tool: &Tool) -> Result<Value> {
        let supports = flag(self.model, "supportsStrictTools", false)
            && !(self.disable_preferred && prefers(tool));
        let (schema, strict) = strict_parameters(tool, supports, no_strict_keyword)?;
        let mut input = if strict == Some(true) {
            schema.as_ref().clone()
        } else {
            json!({"type":"object"})
        };
        for key in [
            "properties",
            "required",
            "additionalProperties",
            "$defs",
            "definitions",
        ] {
            if let Some(value) = schema.get(key).filter(|v| !v.is_null()) {
                input[key] = value.clone();
            }
        }
        if input["properties"].is_null() {
            input["properties"] = json!({});
        }
        if input["required"].is_null() {
            input["required"] = json!([]);
        }
        let mut wire = json!({"name":oauth_tool(&tool.name,self.oauth),"description":tool.description,"input_schema":input});
        if strict == Some(true) {
            wire["strict"] = json!(true);
            self.saw_preferred |= prefers(tool);
        }
        if flag(self.model, "supportsEagerToolInputStreaming", true) {
            wire["eager_input_streaming"] = json!(true);
        }
        Ok(wire)
    }
    fn messages(
        &mut self,
        messages: Vec<Message>,
        cache: Option<&Value>,
        managed: bool,
        native: bool,
        final_effort: &str,
    ) -> Result<Vec<Value>> {
        let messages = transform_messages(messages, self.model, |id, _, _| Some(normalize(id)));
        let mut pending = Vec::new();
        let mut output = Vec::with_capacity(messages.len());
        let mut levels = IndexMap::new();
        let mut iter = messages.into_iter().peekable();
        while let Some(message) = iter.next() {
            match message {
                Message::System(s) => {
                    let text = render_system_message_update(&s);
                    let mut content = Vec::new();
                    if !text.is_empty() {
                        content.push(json!({"type":"text","text":text}));
                    }
                    if native {
                        let added = s.tools_added.as_deref().unwrap_or_default();
                        for old in s
                            .tools_removed
                            .iter()
                            .flatten()
                            .filter(|old| !added.iter().any(|t| t.name == old.name))
                        {
                            content.push(json!({"type":"tool_removal","tool":{"type":"tool_reference","name":oauth_tool(&old.name,self.oauth)}}));
                        }
                        for tool in added {
                            content.push(json!({"type":"tool_addition","tool":{"type":"tool_definition","definition":self.tool(tool)?}}));
                        }
                    }
                    if !content.is_empty() {
                        pending.push(json!({"role":"system","content":content}));
                    }
                }
                Message::User(u) => {
                    match u.content {
                        StringOrBlocks::String(s) if !s.trim().is_empty() => {
                            output.push(json!({"role":"user","content":s}))
                        }
                        StringOrBlocks::Blocks(items) => {
                            let content:Vec<_>=items.iter().filter(|b|!matches!(b,InputContent::Text(t) if t.text.trim().is_empty())).map(part).collect();
                            if !content.is_empty() {
                                output.push(json!({"role":"user","content":content}));
                            }
                        }
                        _ => {}
                    }
                }
                Message::Assistant(a) => {
                    let content = assistant_blocks(
                        &a,
                        self.oauth,
                        flag(self.model, "allowEmptySignature", false),
                    );
                    if content.is_empty() {
                        continue;
                    }
                    output.append(&mut pending);
                    if managed
                        && a.provider == self.model.provider
                        && a.api == Api::AnthropicMessages
                        && let Some(effort) = a.provider_thinking_level.filter(|s| {
                            matches!(s.as_str(), "low" | "medium" | "high" | "xhigh" | "max")
                        })
                    {
                        levels.insert(output.len(), effort);
                    }
                    output.push(json!({"role":"assistant","content":content}));
                }
                Message::ToolResult(mut tool) => {
                    let mut tools = Vec::new();
                    let mut after = Vec::new();
                    loop {
                        let content = if let Some(split) = split_error_tool_result_images(&tool) {
                            after.push(json!({"type":"text","text":split.caption}));
                            after.extend(split.images.iter().map(image));
                            Value::String(
                                split
                                    .content
                                    .iter()
                                    .map(|t| t.text.as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            )
                        } else {
                            tool_result_content(&tool.content)
                        };
                        tools.push(json!({"type":"tool_result","tool_use_id":tool.tool_call_id,"content":content,"is_error":tool.is_error}));
                        if matches!(iter.peek(), Some(Message::ToolResult(_)))
                            && let Some(Message::ToolResult(next)) = iter.next()
                        {
                            tool = next;
                        } else {
                            break;
                        }
                    }
                    tools.append(&mut after);
                    output.push(json!({"role":"user","content":tools}));
                }
            }
        }
        output.append(&mut pending);
        if let Some(last) = output
            .last_mut()
            .filter(|last| matches!(last["role"].as_str(), Some("user" | "system")))
            && let Some(cc) = cache
        {
            let raw = &mut last["content"];
            if raw.is_string() {
                let text = raw.take();
                *raw = json!([{"type":"text","text":text,"cache_control":cc}]);
            } else if let Some(last) = raw.as_array_mut().and_then(|p| p.last_mut())
                && matches!(
                    last["type"].as_str(),
                    Some("text" | "image" | "tool_result" | "tool_addition" | "tool_removal")
                )
            {
                last["cache_control"] = cc.clone();
            }
        }
        if managed {
            let mut updates = Vec::with_capacity(output.len() + levels.len() + 1);
            for (i, part) in output.into_iter().enumerate() {
                if let Some(effort) = levels.shift_remove(&i) {
                    updates.push(effort_update(&effort));
                }
                updates.push(part);
            }
            updates.push(effort_update(final_effort));
            Ok(updates)
        } else {
            Ok(output)
        }
    }
}
fn effort_update(effort: &str) -> Value {
    json!({"role":"system","content":[],"output_config":{"effort":effort}})
}
struct Body {
    value: Value,
    effort: Option<String>,
    preferred: bool,
}
fn build(
    model: &Model,
    ctx: TranscriptContext,
    opts: &StreamOptions,
    oauth: bool,
    threads: bool,
    disable: bool,
) -> Result<Body> {
    let mid = flag(model, "supportsMidConvoSystemMessages", false);
    let ctx = resolve_transcript(ctx, mid);
    let mut builder = Builder {
        model,
        oauth,
        disable_preferred: disable,
        saw_preferred: false,
    };
    let system = get_initial_system_message(ctx.messages());
    let initial = system
        .and_then(|s| s.tools_added.as_deref())
        .unwrap_or_default();
    let current = get_current_tools(ctx.messages());
    let native = mid && flag(model, "supportsMidConvoToolChanges", false) && !initial.is_empty();
    let requested = opts
        .specific
        .get("thinkingEnabled")
        .and_then(Value::as_bool);
    let no_thinking = model.reasoning
        && requested == Some(false)
        && model
            .thinking_level_map
            .as_ref()
            .and_then(|m| m.get(&ThinkingLevel::Off))
            != Some(&None);
    let mid_effort = flag(model, "supportsMidConvoEffort", false);
    let adaptive = flag(model, "forceAdaptiveThinking", false);
    let managed = mid_effort && !no_thinking;
    let effort = opts
        .specific
        .get("effort")
        .and_then(Value::as_str)
        .unwrap_or("high");
    let display = opts
        .specific
        .get("thinkingDisplay")
        .and_then(Value::as_str)
        .unwrap_or("summarized");
    let retention = retention(opts);
    let cc = match retention {
        CacheRetention::None => None,
        CacheRetention::Long if flag(model, "supportsLongCacheRetention", true) => {
            Some(json!({"type":"ephemeral","ttl":"1h"}))
        }
        _ => Some(json!({"type":"ephemeral"})),
    };
    let mut top_tools = if native {
        initial
            .iter()
            .map(|t| builder.tool(t))
            .collect::<Result<Vec<_>>>()?
    } else {
        current
            .iter()
            .map(|t| builder.tool(t))
            .collect::<Result<Vec<_>>>()?
    };
    if flag(model, "supportsCacheControlOnTools", true)
        && let Some(last) = top_tools.last_mut()
        && let Some(cache) = &cc
    {
        last["cache_control"] = cache.clone();
    }
    if native {
        top_tools.push(json!({"name":"__pi_deferred_placeholder__","description":"Reserved placeholder. Never available. Never call this.","input_schema":{"type":"object","properties":{},"required":[]},"defer_loading":true}));
    }
    let mut features = IndexSet::new();
    let mut override_header = model
        .headers
        .as_ref()
        .and_then(|h| {
            h.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
        })
        .map(|(_, v)| Some(v.as_str()));
    if let Some((_, v)) = opts
        .request
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
    {
        override_header = Some(v.as_deref());
    }
    let fallback = model
        .compat("allowedFallbackModels")
        .as_array()
        .filter(|a| !a.is_empty());
    if let Some(forced) = override_header {
        if let Some(forced) = forced {
            features.extend(forced.split(',').map(str::trim).filter(|s| !s.is_empty()));
        }
    } else {
        if oauth {
            features.extend(["claude-code-20250219", "oauth-2025-04-20"]);
        }
        if !current.is_empty() && !flag(model, "supportsEagerToolInputStreaming", true) {
            features.insert(EAGER_BETA);
        }
        if model.reasoning
            && requested == Some(true)
            && opts
                .specific
                .get("interleavedThinking")
                .and_then(Value::as_bool)
                != Some(false)
            && !adaptive
        {
            features.insert(INTERLEAVED);
        }
        if fallback.is_some() {
            features.insert(FALLBACK_BETA);
        }
        if mid_effort {
            features.extend([MID_EFFORT, THINKING_BINDING]);
        }
        if native {
            features.insert(INLINE_TOOLS);
        }
        if threads {
            features.insert(THREAD_BETA);
        }
    }
    let mut system_blocks = Vec::new();
    let mut sys_block = |text: &str| {
        let mut block = json!({"type":"text","text":text});
        if let Some(cc) = &cc {
            block["cache_control"] = cc.clone();
        }
        system_blocks.push(block);
    };
    if oauth {
        sys_block("You are Claude Code, Anthropic's official CLI for Claude.");
    }
    if let Some(text) = system.map(system_message_text).filter(|s| !s.is_empty()) {
        sys_block(&text);
    }
    let skip = usize::from(system.is_some());
    let mut messages = ctx.into_messages();
    if skip == 1 {
        messages.remove(0);
    }
    let messages = builder.messages(messages, cc.as_ref(), managed, native, effort)?;
    let mut value = json!({"model":model.id,"messages":messages,"max_tokens":opts.max_tokens.unwrap_or(model.max_tokens),"stream":true});
    if !features.is_empty() {
        value["betas"] = json!(features);
    }
    if !system_blocks.is_empty() {
        value["system"] = json!(system_blocks);
    }
    if !top_tools.is_empty() {
        value["tools"] = json!(top_tools);
    }
    if requested != Some(true)
        && !mid_effort
        && flag(model, "supportsTemperature", true)
        && let Some(temp) = opts.temperature
    {
        value["temperature"] = json!(temp);
    }
    if no_thinking {
        value["thinking"] = json!({"type":"disabled"});
    } else if managed {
        value["thinking"] = json!({"type":"adaptive","display":display,"block_binding":{"prefix_mismatch_behavior":"drop_block"}});
        value["output_config"] = json!({"effort":"low"});
    } else if model.reasoning && requested == Some(true) {
        if adaptive {
            value["thinking"] = json!({"type":"adaptive","display":display});
            if opts.specific.contains_key("effort") {
                value["output_config"] = json!({"effort":effort});
            }
        } else {
            value["thinking"] = json!({"type":"enabled","budget_tokens":opts.specific.get("thinkingBudgetTokens").and_then(Value::as_u64).filter(|n|*n>0).unwrap_or(1024),"display":display});
        }
    }
    if let Some(user) = opts.metadata.get("user_id").and_then(Value::as_str) {
        value["metadata"] = json!({"user_id":user});
    }
    if let Some(choice) = opts.specific.get("toolChoice") {
        value["tool_choice"] = if let Some(choice) = choice.as_str() {
            json!({"type":choice})
        } else {
            choice.clone()
        };
    } else if let Some(choice) = opts.tool_choice {
        value["tool_choice"] = json!({"type":choice});
    }
    if let Some(fallback) = fallback {
        value["fallbacks"] = json!(
            fallback
                .iter()
                .filter_map(|b| b.get("model").map(|model| json!({"model":model})))
                .collect::<Vec<_>>()
        );
    }
    Ok(Body {
        value,
        effort: managed.then(|| effort.to_owned()),
        preferred: builder.saw_preferred,
    })
}

fn thread_hash(model: &Model, body: &Value, messages: &[Value]) -> String {
    fn simplify(v: &mut Value) {
        match v {
            Value::Object(fields) => {
                fields.shift_remove("cache_control");
                for (key, value) in fields {
                    if key == "content" && value.is_string() {
                        let text = value.take();
                        *value = json!([{"type":"text","text":text}]);
                    } else {
                        simplify(value);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(simplify),
            _ => {}
        }
    }
    let mut messages = messages.to_vec();
    messages.iter_mut().for_each(simplify);
    let stable = json!([
        model.provider,
        model.base_url,
        body["model"],
        body["system"],
        body["tools"],
        body["thinking"],
        body["output_config"],
        body["tool_choice"],
        body["betas"],
        body["fallbacks"]
    ]);
    let mut prefix = stable.to_string();
    prefix.push_str(&Value::Array(messages).to_string());
    short_hash(&prefix)
}
fn prepare_thread(model: &Model, session: &str, body: &Value) -> Option<Value> {
    let mut entries = THREADS.lock().unwrap_or_else(|e| e.into_inner());
    if matches!(entries.get(session), Some(None)) {
        return None;
    }
    let messages = body["messages"].as_array()?;
    let found = entries.get(session).and_then(Option::as_ref).filter(|e| {
        messages.len() > e.count && thread_hash(model, body, &messages[..e.count]) == e.hash
    });
    let mut thread = body.clone();
    if let Some(e) = found {
        thread["thread"] = json!({"type":"continue","previous_message_id":e.id});
        thread["messages"] = json!(messages[e.count..]);
        if let Some(fields) = thread.as_object_mut() {
            fields.shift_remove("tools");
            fields.shift_remove("system");
        }
        entries.shift_remove(session);
    } else {
        thread["thread"] = json!({"type":"create"});
    }
    Some(thread)
}
fn save_thread(session: &str, entry: Option<ThreadEntry>) {
    let mut entries = THREADS.lock().unwrap_or_else(|e| e.into_inner());
    entries.shift_remove(session);
    entries.insert(session.to_owned(), entry);
    if entries.len() > MAX_CACHE {
        entries.shift_remove_index(0);
    }
}
fn diagnostic(state: &ChatState, kind: &str, error: &Error, reason: &str) {
    let d = json!({"type":kind,"timestamp":unix_millis(),"error":{"message":error.to_string()},"details":{"reason":reason}});
    if let Value::Object(d) = d {
        write_partial(&state.message)
            .diagnostics
            .get_or_insert_with(Vec::new)
            .push(d);
    }
}
struct FederationConfig {
    rule: String,
    org: String,
    path: String,
    service: Option<String>,
    workspace: Option<String>,
}
fn federation_config(model: &Model, opts: &StreamOptions) -> Option<FederationConfig> {
    if model.provider != "anthropic" {
        return None;
    }
    let env = |key| provider_env_value(key, &opts.request.env).map(std::borrow::Cow::into_owned);
    Some(FederationConfig {
        rule: env("ANTHROPIC_FEDERATION_RULE_ID")?,
        org: env("ANTHROPIC_ORGANIZATION_ID")?,
        path: env("ANTHROPIC_IDENTITY_TOKEN_FILE")?,
        service: env("ANTHROPIC_SERVICE_ACCOUNT_ID"),
        workspace: env("ANTHROPIC_WORKSPACE_ID"),
    })
}
async fn federation_token(model: &Model, opts: &StreamOptions) -> Result<Option<String>> {
    let Some(FederationConfig {
        rule,
        org,
        path,
        service,
        workspace,
    }) = federation_config(model, opts)
    else {
        return Ok(None);
    };
    let url = endpoint(&model.base_url, "v1/oauth/token")?;
    if url.scheme() != "https"
        && !url
            .host_str()
            .is_some_and(|h| matches!(h, "localhost" | "127.0.0.1" | "[::1]"))
    {
        return Err(Error::Auth(
            "Anthropic federation token endpoint requires HTTPS".into(),
        ));
    }
    let jwt = tokio::fs::read_to_string(path).await?;
    let jwt = jwt.trim();
    if jwt.is_empty() || jwt.len() > 16 * 1024 {
        return Err(Error::Auth(
            "Invalid Anthropic federation token length".into(),
        ));
    }
    let mut body = json!({"grant_type":"urn:ietf:params:oauth:grant-type:jwt-bearer","assertion":jwt,"federation_rule_id":rule,"organization_id":org});
    if let Some(service) = service {
        body["service_account_id"] = json!(service);
    }
    if let Some(workspace) = workspace {
        body["workspace_id"] = json!(workspace);
    }
    let client = crate::http::client_for(&url, &opts.request, false)?;
    let token = abortable(&opts.request.signal, async {
        let response = client
            .post(url)
            .header(
                "anthropic-beta",
                "oauth-2025-04-20,oidc-federation-2026-04-01",
            )
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Error::Auth(format!(
                "Anthropic federation token exchange failed ({})",
                response.status()
            )));
        }
        let token: Value = response.json().await?;
        token["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                Error::Auth("Anthropic federation response omitted the access token".into())
            })
    })
    .await?;
    Ok(Some(token))
}
async fn request(
    model: &Model,
    opts: &StreamOptions,
    ctx: &TranscriptContext,
    body: &Value,
    oauth: bool,
    bearer: Option<&str>,
) -> Result<Response> {
    let mut headers = ProviderHeaders::from([
        ("accept".into(), Some("application/json".into())),
        ("content-type".into(), Some("application/json".into())),
        ("anthropic-version".into(), Some("2023-06-01".into())),
        (
            "anthropic-dangerous-direct-browser-access".into(),
            Some("true".into()),
        ),
    ]);
    if oauth {
        headers.insert("user-agent".into(), Some("claude-cli/2.1.322".into()));
        headers.insert("x-app".into(), Some("cli".into()));
    }
    if let Some(token) = bearer {
        headers.insert("authorization".into(), Some(format!("Bearer {token}")));
    } else if let Some(key) = &opts.request.api_key {
        headers.insert("x-api-key".into(), Some(key.clone()));
    }
    if let Some(betas) = body["betas"].as_array() {
        headers.insert(
            "anthropic-beta".into(),
            Some(
                betas
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        );
    }
    let openrouter = model.provider == "openrouter" || model.base_url.contains("openrouter.ai");
    if !oauth
        && model.provider != "github-copilot"
        && flag(model, "sendSessionAffinityHeaders", openrouter)
        && retention(opts) != CacheRetention::None
        && let Some(session) = opts.session_id.as_ref()
    {
        let name = if model
            .compat("sessionAffinityFormat")
            .as_str()
            .map_or(openrouter, |s| s == "openrouter")
        {
            "x-session-id"
        } else {
            "x-session-affinity"
        };
        headers.insert(name.into(), Some(session.clone()));
    }
    copilot_headers(model, ctx, &mut headers);
    let mut path = endpoint(&model.base_url, "v1/messages")?;
    path.query_pairs_mut().append_pair("beta", "true");
    let mut body = body.clone();
    if let Some(m) = body.as_object_mut() {
        m.shift_remove("betas");
    }
    send(
        HttpRequest {
            method: Method::POST,
            url: path,
            headers: request_headers(&model.base, &opts.request, headers)?,
            body: Some(serde_json::to_vec(&body)?),
            error_label: "Anthropic API error".into(),
            force_http1: false,
            default_retries: 0,
            no_retry_statuses: vec![],
        },
        model,
        &opts.request,
    )
    .await
}
async fn run(
    state: &mut ChatState,
    model: &Model,
    context: TranscriptContext,
    opts: &StreamOptions,
) -> Result<StopReason> {
    if !auth_available(model, opts) {
        return Err(Error::Auth(format!(
            "No API key for provider: {}",
            model.provider
        )));
    }
    let key = opts.request.api_key.as_deref();
    let oauth = model.provider != "github-copilot" && key.is_some_and(oauth_key);
    let federated = if key.is_none() && !auth_header(opts) {
        federation_token(model, opts).await?
    } else {
        None
    };
    let bearer = key
        .filter(|_| oauth || model.provider == "github-copilot")
        .or(federated.as_deref());
    let context = resolve_transcript(
        context,
        flag(model, "supportsMidConvoSystemMessages", false),
    );
    let cache_id = opts.session_id.as_deref().filter(|_| {
        retention(opts) != CacheRetention::None
            && flag(
                model,
                "supportsMessageThreads",
                model.base_url.contains("api.anthropic.com"),
            )
    });
    let mut strict_off = exceeded_strict(opts.session_id.as_deref());
    let (response, thread, body) = loop {
        let candidate = build(
            model,
            context.clone(),
            opts,
            oauth,
            cache_id.is_some(),
            strict_off,
        )?;
        write_partial(&state.message).provider_thinking_level = candidate.effort;
        let mut full = payload(candidate.value, model, &opts.request).await?;
        full["stream"] = Value::Bool(true);
        let mut threaded = cache_id
            .filter(|_| !opts.disable_continuation)
            .and_then(|s| prepare_thread(model, s, &full));
        let mut response = request(
            model,
            opts,
            &context,
            threaded.as_ref().unwrap_or(&full),
            oauth,
            bearer,
        )
        .await;
        if let (
            Some(session),
            Some(_),
            Err(
                error @ Error::Http {
                    status: StatusCode::BAD_REQUEST,
                    message,
                    ..
                },
            ),
        ) = (cache_id, threaded.as_ref(), &response)
        {
            let retry = [
                "thread_not_found",
                "thread_already_continued",
                "thread_continue_in_flight",
                "thread_fingerprint_mismatch",
            ]
            .into_iter()
            .find(|kind| message.contains(kind));
            if let Some(code) = retry {
                diagnostic(state, "anthropic_thread_fallback", error, code);
                let mut recreate = full.clone();
                recreate["thread"] = json!({"type":"create"});
                response = request(model, opts, &context, &recreate, oauth, bearer).await;
                threaded = Some(recreate);
            } else if message.contains("thread") && !strict_limit_error(message) {
                diagnostic(
                    state,
                    "anthropic_thread_fallback",
                    error,
                    "thread_unsupported_request",
                );
                response = request(model, opts, &context, &full, oauth, bearer).await;
                if response.is_ok() {
                    save_thread(session, None);
                }
                threaded = None;
            }
        }
        match response {
            Ok(response) => break (response, threaded.is_some(), full),
            Err(
                error @ Error::Http {
                    status: StatusCode::BAD_REQUEST,
                    ..
                },
            ) if !strict_off && candidate.preferred && strict_limit_error(&error.to_string()) => {
                strict_off = true;
                mark_over_limit(opts.session_id.as_deref());
                diagnostic(
                    state,
                    "anthropic_strict_tools_fallback",
                    &error,
                    "strict_tool_limits_exceeded",
                );
            }
            Err(error) => return Err(error),
        }
    };
    let reason = consume(response, state, model, &context, opts, oauth).await?;
    if thread && let Some(session) = cache_id {
        let message = read_partial(&state.message);
        if let Some(id) = message
            .response_id
            .as_ref()
            .filter(|_| message.response_model.is_none())
            && let Some(previous) = body["messages"].as_array()
        {
            let mut prefix = previous.clone();
            let reply =
                assistant_blocks(&message, oauth, flag(model, "allowEmptySignature", false));
            if !reply.is_empty() {
                prefix.push(json!({"role":"assistant","content":reply}));
            }
            save_thread(
                session,
                Some(ThreadEntry {
                    count: prefix.len(),
                    hash: thread_hash(model, &body, &prefix),
                    id: id.clone(),
                }),
            );
        }
    }
    Ok(reason)
}

fn update_usage(usage: &mut Usage, value: &Value, cost: &ModelCost, initial: bool) {
    let n = |key| value[key].as_f64();
    if let Some(v) = n("input_tokens") {
        usage.input = v;
    }
    if let Some(v) = n("output_tokens") {
        usage.output = v;
    }
    if let Some(v) = n("cache_read_input_tokens") {
        usage.cache_read = v;
    }
    if let Some(v) = n("cache_creation_input_tokens") {
        usage.cache_write = v;
    }
    if let Some(v) = value["cache_creation"]["ephemeral_1h_input_tokens"].as_f64() {
        usage.cache_write_1h = Some(v);
    } else if initial {
        usage.cache_write_1h = Some(0.0);
    }
    if let Some(v) = value["output_tokens_details"]["thinking_tokens"].as_f64() {
        usage.reasoning = Some(v);
    }
    usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
    usage.calculate_cost(cost);
}
struct Block {
    index: usize,
    kind: &'static str,
}
async fn consume(
    response: Response,
    state: &mut ChatState,
    model: &Model,
    ctx: &TranscriptContext,
    opts: &StreamOptions,
    oauth: bool,
) -> Result<StopReason> {
    state.start();
    let mut frames = SseReader::new(response.bytes_stream());
    let mut blocks = HashMap::<u64, Block>::new();
    let mut started = false;
    let mut stopped = false;
    let mut finish = None;
    let mut fallback_cost: Option<ModelCost> = None;
    let mut transformations = None;
    let tools = get_current_tools(ctx.messages());
    while let Some(frame) = frames.next(&opts.request.signal).await? {
        if frame.name == "error" {
            return Err(Error::Protocol(frame.data));
        }
        if !matches!(
            frame.name.as_str(),
            "message_start"
                | "content_block_start"
                | "content_block_delta"
                | "content_block_stop"
                | "message_delta"
                | "message_stop"
        ) {
            continue;
        }
        let event: Value =
            crate::streaming_json::parse_json_with_repair(&frame.data).map_err(|error| {
                Error::Protocol(format!(
                    "Could not parse Anthropic SSE event {}: {error}; data={}",
                    frame.name, frame.data
                ))
            })?;
        let kind = event["type"].as_str().unwrap_or_default();
        if let Some(watch) = &opts.on_provider_stream_event {
            abortable(&opts.request.signal, watch(&event, model)).await?;
        }
        let index = event["index"].as_u64().unwrap_or(u64::MAX);
        match kind {
            "message_start" => {
                let raw = &event["message"];
                started = true;
                let mut msg = write_partial(&state.message);
                msg.response_id = raw["id"].as_str().map(str::to_owned);
                if let Some(name) = raw["model"].as_str().filter(|name| *name != model.id) {
                    msg.response_model = Some(name.into());
                    fallback_cost = model
                        .compat("allowedFallbackModels")
                        .as_array()
                        .and_then(|a| {
                            a.iter()
                                .find(|m| m["provider"] == model.provider && m["model"] == name)
                        })
                        .and_then(|m| serde_json::from_value(m["cost"].clone()).ok());
                }
                update_usage(
                    &mut msg.usage,
                    &raw["usage"],
                    fallback_cost.as_ref().unwrap_or(&model.cost),
                    true,
                );
                if let Some(arr) = raw["input_transformations"].as_array() {
                    transformations = Some(arr.clone());
                }
            }
            "content_block_start" => {
                let b = &event["content_block"];
                let (i, kind) = match b["type"].as_str() {
                    Some("fallback") => {
                        if !read_partial(&state.message).content.is_empty() {
                            return Err(Error::Protocol(
                                "Anthropic performed an unsupported mid-output model fallback"
                                    .into(),
                            ));
                        }
                        continue;
                    }
                    Some("text") => {
                        let i = state.text_start(None);
                        if let Some(text) = b["text"].as_str() {
                            state.text_update(i, |x| x.text = text.into());
                        }
                        (i, "text")
                    }
                    Some("thinking") => (
                        state.thinking_start(ThinkingContent {
                            thinking: b["thinking"].as_str().unwrap_or_default().into(),
                            thinking_signature: Some(
                                b["signature"].as_str().unwrap_or_default().into(),
                            ),
                            ..ThinkingContent::default()
                        }),
                        "thinking",
                    ),
                    Some("redacted_thinking") => (
                        state.thinking_start(ThinkingContent {
                            thinking: "[Reasoning redacted]".into(),
                            thinking_signature: b["data"].as_str().map(str::to_owned),
                            redacted: Some(true),
                            ..ThinkingContent::default()
                        }),
                        "thinking",
                    ),
                    Some("tool_use") => {
                        let name = b["name"].as_str().unwrap_or_default();
                        let name = if oauth {
                            tools
                                .iter()
                                .find(|t| t.name.eq_ignore_ascii_case(name))
                                .map_or(name, |t| t.name.as_str())
                        } else {
                            name
                        };
                        (
                            state.tool_start(ToolCall {
                                id: b["id"].as_str().unwrap_or_default().into(),
                                name: name.into(),
                                arguments: b["input"].as_object().cloned().unwrap_or_default(),
                                ..ToolCall::default()
                            }),
                            "tool",
                        )
                    }
                    _ => continue,
                };
                blocks.insert(index, Block { index: i, kind });
            }
            "content_block_delta" => {
                if let Some(block) = blocks.get(&index) {
                    let delta = &event["delta"];
                    let field = match (delta["type"].as_str(), block.kind) {
                        (Some("text_delta"), "text") => "text",
                        (Some("thinking_delta"), "thinking") => "thinking",
                        (Some("input_json_delta"), "tool") => "partial_json",
                        (Some("signature_delta"), "thinking") => {
                            if let Some(signature) = delta["signature"].as_str() {
                                state.thinking_update(block.index, |b| {
                                    b.thinking_signature
                                        .get_or_insert_with(String::new)
                                        .push_str(signature)
                                });
                            }
                            continue;
                        }
                        _ => continue,
                    };
                    if let Some(s) = delta[field].as_str() {
                        state.delta(block.index, s.into())?;
                    }
                }
            }
            "content_block_stop" => {
                if let Some(b) = blocks.remove(&index) {
                    state.end(b.index);
                }
            }
            "message_delta" => {
                let delta = &event["delta"];
                let mut msg = write_partial(&state.message);
                if let Some(raw) = delta["stop_reason"].as_str() {
                    msg.raw_stop_reason = Some(raw.into());
                    finish = Some(match raw {
                        "end_turn" | "pause_turn" | "stop_sequence" => Ok(StopReason::Stop),
                        "tool_use" => Ok(StopReason::ToolUse),
                        "max_tokens" => Ok(StopReason::Length),
                        "refusal" => Err(Error::Protocol(
                            delta["stop_details"]["explanation"]
                                .as_str()
                                .unwrap_or("The model refused to complete the request")
                                .into(),
                        )),
                        "sensitive" => {
                            Err(Error::Protocol("Provider stopped with: sensitive".into()))
                        }
                        _ => Err(Error::Protocol(format!("Unhandled stop reason: {raw}"))),
                    });
                }
                if let Some(arr) = event["input_transformations"].as_array() {
                    transformations = Some(arr.clone());
                }
                update_usage(
                    &mut msg.usage,
                    &event["usage"],
                    fallback_cost.as_ref().unwrap_or(&model.cost),
                    false,
                );
                if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
                    apply_openrouter_cost(&mut msg.usage, &event["usage"]);
                }
            }
            "message_stop" => stopped = true,
            _ => {}
        }
    }
    if started && !stopped {
        return Err(Error::Protocol(
            "Anthropic stream ended before message_stop".into(),
        ));
    }
    let reason = finish
        .ok_or_else(|| Error::Protocol("Anthropic stream ended without a stop reason".into()))??;
    if let Some(items) = transformations.filter(|items| !items.is_empty()) {
        let d = json!({"type":"anthropic_input_transformations","timestamp":unix_millis(),"details":{"transformations":items}});
        if let Value::Object(d) = d {
            write_partial(&state.message)
                .diagnostics
                .get_or_insert_with(Vec::new)
                .push(d);
        }
    }
    Ok(reason)
}

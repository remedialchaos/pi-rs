use std::collections::HashMap;

use indexmap::IndexSet;
use reqwest::{Method, Response};
use serde_json::{Value, json};
use url::Url;

use crate::chat_state::ChatState;
use crate::constrained::{JsonGrammarInput, grammar, strict_parameters};
use crate::error::{Error, Result};
use crate::event_stream::{AssistantMessageEventStream, read_partial, write_partial};
use crate::http::{HttpRequest, abortable, endpoint, payload, request_headers, send};
use crate::model::{Api, CacheRetention, Model, ThinkingLevel};
use crate::openai_compat::short_hash;
use crate::openai_completions::{api_key, copilot_headers, retention};
use crate::openrouter_images::image_data_url;
use crate::request::{ProviderHeaders, StreamOptions, provider_env_value};
use crate::simple_options::{build_base_options, resolve_sampling_params};
use crate::sse::SseReader;
use crate::transcript::{
    TranscriptToolSupport, get_declared_tools, input_text, render_system_message_update,
    resolve_transcript, resolve_transcript_tools, system_message_text,
};
use crate::transform::transform_messages;
use crate::types::{
    AssistantContent, AssistantMessage, InputContent, Message, StopReason, StringOrBlocks,
    ThinkingContent, Tool, ToolCall, TranscriptContext, Usage,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dialect {
    OpenAi,
    Azure,
    Codex,
}
impl Dialect {
    fn from_model(model: &Model) -> Self {
        if model.api == Api::AzureOpenAiResponses {
            Self::Azure
        } else {
            Self::OpenAi
        }
    }
    fn supports_pricing(self) -> bool {
        self != Self::Azure
    }
}

pub fn stream(
    model: Model,
    context: TranscriptContext,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    let (mut state, stream) = ChatState::new(&model);
    tokio::spawn(async move {
        let result = run(&model, context, &options, &mut state)
            .await
            .map_err(chatgpt_error);
        state.finish(result);
    });
    stream
}
pub fn stream_simple(
    model: Model,
    context: TranscriptContext,
    options: StreamOptions,
) -> Result<AssistantMessageEventStream> {
    request_key(&model, &options, Dialect::from_model(&model))?;
    let options = build_base_options(&model, &context, options);
    Ok(stream(model, context, options))
}
fn request_key<'a>(model: &Model, options: &'a StreamOptions, d: Dialect) -> Result<&'a str> {
    if d == Dialect::OpenAi {
        return api_key(model, options);
    }
    options
        .request
        .api_key
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Auth(format!("No API key for provider: {}", model.provider)))
}
fn azure_opt<'a>(
    options: &'a StreamOptions,
    name: &str,
    env: &str,
) -> Option<std::borrow::Cow<'a, str>> {
    options
        .specific
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| std::borrow::Cow::Borrowed(s.trim()))
        .or_else(|| provider_env_value(env, &options.request.env))
}
pub fn resolve_azure_url(model: &Model, opts: &StreamOptions) -> Result<Url> {
    let base = azure_opt(opts, "azureBaseUrl", "AZURE_OPENAI_BASE_URL");
    let resource = azure_opt(opts, "azureResourceName", "AZURE_OPENAI_RESOURCE_NAME");
    let raw = if let Some(base) = base.as_deref().filter(|s| !s.trim().is_empty()) {
        std::borrow::Cow::Borrowed(base)
    } else if let Some(resource) = resource {
        std::borrow::Cow::Owned(format!("https://{resource}.openai.azure.com/openai/v1"))
    } else if !model.base_url.is_empty() {
        std::borrow::Cow::Borrowed(model.base_url.as_str())
    } else {
        return Err(Error::Protocol("Azure OpenAI base URL is required. Set AZURE_OPENAI_BASE_URL or AZURE_OPENAI_RESOURCE_NAME, or pass azureBaseUrl, azureResourceName, or model.baseUrl.".into()));
    };
    let mut url = Url::parse(raw.trim().trim_end_matches('/'))
        .map_err(|_| Error::Protocol(format!("Invalid Azure OpenAI base URL: {raw}")))?;
    let host = url.host_str().unwrap_or_default();
    let azure = [
        ".openai.azure.com",
        ".cognitiveservices.azure.com",
        ".ai.azure.com",
    ]
    .into_iter()
    .any(|suffix| host.ends_with(suffix));
    if azure
        && matches!(
            url.path().trim_end_matches('/'),
            "" | "/openai" | "/openai/v1/responses"
        )
    {
        url.set_path("/openai/v1");
        url.set_query(None);
    }
    let mut url = endpoint(url.as_str(), "responses")?;
    let version = azure_opt(opts, "azureApiVersion", "AZURE_OPENAI_API_VERSION");
    url.query_pairs_mut()
        .append_pair("api-version", version.as_deref().unwrap_or("v1"));
    Ok(url)
}
pub(crate) fn azure_deployment<'m>(
    model: &'m Model,
    opts: &'m StreamOptions,
) -> std::borrow::Cow<'m, str> {
    if let Some(name) = opts
        .specific
        .get("azureDeploymentName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return name.into();
    }
    if let Some(map) = provider_env_value("AZURE_OPENAI_DEPLOYMENT_NAME_MAP", &opts.request.env)
        && let Some(name) = map
            .rsplit(',')
            .filter_map(|v| v.trim().split_once('='))
            .find_map(|(id, name)| {
                let name = name.trim();
                (id.trim() == model.id && !name.is_empty()).then_some(name)
            })
    {
        return name.to_owned().into();
    }
    model.id.as_str().into()
}
fn chatgpt_error(error: Error) -> Error {
    if let Error::Http {
        status,
        headers,
        mut message,
    } = error
    {
        if message.contains("subscription_sharing_usage_limit_exceeded") {
            message.push_str("\nCheck your ChatGPT usage: https://chatgpt.com/settings/usage");
        } else if message.contains("subscription_sharing_workplace_individual_usage_limit_exceeded")
        {
            message.push_str("\nYou've reached your ChatGPT workspace's usage limit. Ask your workspace admin to increase it at admin.openai.com → Workspace → Usage limits.");
        }
        Error::Http {
            status,
            headers,
            message,
        }
    } else {
        error
    }
}

fn tool_wire(
    tool: &Tool,
    strict: bool,
    supports_grammar: bool,
    d: Dialect,
    deferred: bool,
) -> Result<Value> {
    let mut body = if let Some(g) = grammar(tool, supports_grammar)? {
        json!({"type":"custom","name":tool.name,"description":tool.description,"format":{"type":"grammar","syntax":g.syntax,"definition":g.definition}})
    } else {
        let (params, enabled) = strict_parameters(tool, strict, |_, _| false)?;
        let mut body = json!({"type":"function","name":tool.name,"description":tool.description,"parameters":params.as_ref()});
        if strict {
            body["strict"] = enabled.map(Value::Bool).unwrap_or(if d == Dialect::Codex {
                Value::Null
            } else {
                Value::Bool(false)
            });
        }
        body
    };
    if deferred {
        body["defer_loading"] = Value::Bool(true);
    }
    Ok(body)
}
fn normalize_part(raw: &str) -> String {
    let clean = raw
        .encode_utf16()
        .take(64)
        .map(|u| {
            let c = char::from_u32(u32::from(u)).unwrap_or('_');
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    clean.trim_end_matches('_').to_owned()
}
fn normalize_id(id: &str, model: &Model, source: &AssistantMessage, d: Dialect) -> Option<String> {
    let allowed = match d {
        Dialect::Azure => matches!(
            model.provider.as_str(),
            "azure" | "openai" | "openai-codex" | "opencode"
        ),
        _ => matches!(
            model.provider.as_str(),
            "openai" | "openai-codex" | "opencode"
        ),
    };
    if allowed && let Some((call, item)) = id.split_once('|') {
        let call = normalize_part(call);
        let item = item.split('|').next().unwrap_or_default();
        let mut item = if source.provider != model.provider || source.api != model.api {
            format!("fc_{}", short_hash(item))
        } else {
            normalize_part(item)
        };
        if !item.starts_with("fc_") {
            item = normalize_part(&format!("fc_{item}"));
        }
        Some(format!("{call}|{item}"))
    } else {
        Some(normalize_part(id))
    }
}
fn converter_input(block: InputContent) -> Value {
    match block {
        InputContent::Text(t) => json!({"type":"input_text","text":t.text}),
        InputContent::Image(image) => {
            json!({"type":"input_image","detail":"auto","image_url":image_data_url(&image)})
        }
    }
}
fn text_signature(signature: Option<String>, fallback: String) -> (String, Option<String>) {
    let Some(s) = signature.filter(|s| !s.is_empty()) else {
        return (fallback, None);
    };
    let (id, phase) = if s.starts_with('{')
        && let Ok(v) = serde_json::from_str::<Value>(&s)
        && v["v"] == 1
        && let Some(id) = v["id"].as_str()
    {
        (
            id.to_owned(),
            v["phase"]
                .as_str()
                .filter(|p| matches!(*p, "commentary" | "final_answer"))
                .map(str::to_owned),
        )
    } else {
        (s, None)
    };
    let id = if id.is_empty() {
        fallback
    } else if id.encode_utf16().count() > 64 {
        format!("msg_{}", short_hash(&id))
    } else {
        id
    };
    (id, phase)
}

pub(crate) fn replay_assistant(
    a: &AssistantMessage,
    model: &Model,
    fields: &HashMap<String, String>,
    msg_index: usize,
) -> Result<Vec<Value>> {
    let same = a.provider == model.provider && a.api == model.api;
    let different = same && a.model != model.id;
    let mut text_index = 0;
    let mut reasoning_index = 0;
    let replay_plain_reasoning = model
        .compat
        .as_ref()
        .and_then(|c| c.get("requiresReasoningContentOnAssistantMessages"))
        .and_then(Value::as_bool)
        == Some(true);
    let mut parts = Vec::with_capacity(a.content.len());
    for block in &a.content {
        match block {
            AssistantContent::Thinking(t) => {
                if let Some(signature) = &t.thinking_signature {
                    parts.push(serde_json::from_str(signature)?);
                } else if replay_plain_reasoning && !t.thinking.is_empty() {
                    let id = if reasoning_index == 0 {
                        format!("rs_pi_{msg_index}")
                    } else {
                        format!("rs_pi_{msg_index}_{reasoning_index}")
                    };
                    reasoning_index += 1;
                    parts.push(json!({
                        "type": "reasoning",
                        "id": id,
                        "summary": [],
                        "content": [{"type": "reasoning_text", "text": t.thinking}],
                    }));
                }
            }
            AssistantContent::Text(t) => {
                let fallback = if text_index == 0 {
                    format!("msg_pi_{msg_index}")
                } else {
                    format!("msg_pi_{msg_index}_{text_index}")
                };
                text_index += 1;
                let (id, phase) = text_signature(t.text_signature.clone(), fallback);
                let mut item = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":t.text,"annotations":[]}],"status":"completed","id":id});
                if let Some(phase) = phase {
                    item["phase"] = json!(phase);
                }
                parts.push(item);
            }
            AssistantContent::ToolCall(t) => {
                let (call, item) =
                    t.id.split_once('|')
                        .map_or((t.id.as_str(), None), |(call, id)| {
                            (call, id.split('|').next())
                        });
                let custom = fields.get(&t.name);
                let mut wire = if let Some(field) = custom {
                    let input = t.arguments.get(field).and_then(Value::as_str).ok_or_else(||
                        Error::Protocol(format!("Grammar tool call \"{}\" requires argument \"{field}\" to be a string.", t.name)))?;
                    json!({"type":"custom_tool_call","call_id":call,"name":t.name,"input":input,"status":"completed"})
                } else {
                    json!({"type":"function_call","call_id":call,"name":t.name,"arguments":serde_json::to_string(&t.arguments)?,"status":"completed"})
                };
                if !different
                    && let Some(item) = item
                        .filter(|id| id.starts_with(if custom.is_some() { "ctc_" } else { "fc_" }))
                {
                    wire["id"] = json!(item);
                }
                if same
                    && !different
                    && let Some(ns) = &t.namespace
                {
                    wire["namespace"] = json!(ns);
                }
                parts.push(wire);
            }
        }
    }
    Ok(parts)
}

struct Conversion<'a> {
    model: &'a Model,
    d: Dialect,
    fields: &'a HashMap<String, String>,
    tool_support: TranscriptToolSupport,
    strict: bool,
    grammar_tools: bool,
    folded: usize,
    effort: Option<&'a str>,
    effort_baseline_initial: bool,
}
impl Conversion<'_> {
    fn tools<'a>(
        &self,
        tools: impl IntoIterator<Item = &'a Tool>,
        deferred: bool,
    ) -> Result<Vec<Value>> {
        tools
            .into_iter()
            .map(|tool| tool_wire(tool, self.strict, self.grammar_tools, self.d, deferred))
            .collect()
    }
    fn convert(&self, context: TranscriptContext) -> Result<(Vec<Value>, Option<String>)> {
        let model = self.model;
        let messages = transform_messages(context.into_messages(), model, |id, m, src| {
            normalize_id(id, m, src, self.d)
        });
        let mut out = Vec::with_capacity(messages.len());
        let mut msg_index = 0;
        let mut system_index = 0;
        let mut declared = false;
        let mut unavailable = IndexSet::<String>::new();
        let role =
            if model.reasoning && model.compat("supportsDeveloperRole").as_bool() != Some(false) {
                "developer"
            } else {
                "system"
            };
        fn config(effort: &str) -> Value {
            json!({"type":"configuration_update","reasoning":{"effort":effort}})
        }
        let mut request_effort_baseline: Option<String> = None;
        let mut previous_effort: Option<String> = None;
        let mut push_config = |effort: &str, out: &mut Vec<Value>| {
            if self.effort_baseline_initial {
                if previous_effort.as_deref() == Some(effort) {
                    return;
                }
                previous_effort = Some(effort.to_owned());
                if request_effort_baseline.is_none() {
                    request_effort_baseline = Some(effort.to_owned());
                    return;
                }
            }
            out.push(config(effort));
        };
        for (source, m) in messages.into_iter().enumerate() {
            let leading = source == 0 && matches!(m, Message::System(_));
            match m {
                Message::System(system) => {
                    let anchored = system_index >= self.folded;
                    system_index += 1;
                    let tools = system.tools_added.as_deref().unwrap_or_default();
                    if anchored && !tools.is_empty() {
                        for tool in tools {
                            unavailable.shift_remove(&tool.name);
                        }
                        match self.tool_support {
                            TranscriptToolSupport::Changes => {
                                if declared {
                                    let mut notice="This is an incremental tools update. Previously declared tools remain available unless explicitly marked unavailable. A tool declared again here is available again, and its latest definition replaces the earlier one.".to_owned();
                                    if !unavailable.is_empty() {
                                        notice.push_str("\nStill unavailable: ");
                                        notice.push_str(
                                            &unavailable
                                                .iter()
                                                .map(String::as_str)
                                                .collect::<Vec<_>>()
                                                .join(", "),
                                        );
                                        notice.push('.');
                                    }
                                    out.push(json!({"role":role,"content":notice}));
                                }
                                declared = true;
                                out.push(json!({"type":"additional_tools","role":"developer","tools":self.tools(tools,false)?}));
                            }
                            TranscriptToolSupport::Additions => {
                                let names =
                                    tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>();
                                let call = format!(
                                    "pi_tool_load_{}",
                                    short_hash(&format!("system:{msg_index}:{}", names.join(",")))
                                );
                                out.push(json!({"type":"tool_search_call","call_id":call,"execution":"client","status":"completed","arguments":{"query":names.join(" "),"limit":names.len()}}));
                                out.push(json!({"type":"tool_search_output","call_id":call,"execution":"client","status":"completed","tools":self.tools(tools,true)?}));
                            }
                            TranscriptToolSupport::None => {}
                        }
                    }
                    let mut text = if leading {
                        system_message_text(&system)
                    } else {
                        render_system_message_update(&system)
                    };
                    if anchored {
                        let removals = system
                            .tools_removed
                            .iter()
                            .flatten()
                            .filter(|r| !tools.iter().any(|t| t.name == r.name))
                            .collect::<Vec<_>>();
                        if !removals.is_empty() {
                            if !text.is_empty() {
                                text.push_str("\n\n");
                            }
                            text.push_str(
                                "The following tools are no longer available. Do not call them:",
                            );
                            for r in removals {
                                text.push_str("\n- ");
                                text.push_str(&r.name);
                                unavailable.insert(r.name.clone());
                            }
                        }
                    }
                    if !text.is_empty() {
                        out.push(json!({"role":role,"content":text}));
                    }
                }
                Message::User(m) => {
                    let content = match m.content {
                        StringOrBlocks::String(s) => vec![json!({"type":"input_text","text":s})],
                        StringOrBlocks::Blocks(parts) => {
                            parts.into_iter().map(converter_input).collect()
                        }
                    };
                    if content.is_empty() {
                        continue;
                    }
                    out.push(json!({"role":"user","content":content}));
                }
                Message::Assistant(a) => {
                    let same = a.provider == model.provider && a.api == model.api;
                    let different = same && a.model != model.id;
                    let parts = replay_assistant(&a, model, self.fields, msg_index)?;
                    if parts.is_empty() {
                        continue;
                    }
                    if self.effort.is_some()
                        && same
                        && !different
                        && let Some(effort) = a.provider_thinking_level.as_deref()
                    {
                        push_config(effort, &mut out);
                    }
                    out.extend(parts);
                }
                Message::ToolResult(t) => {
                    let call = t.tool_call_id.split('|').next().unwrap_or_default();
                    let text = input_text(&t.content, "\n");
                    let has_text = !text.trim().is_empty();
                    let has_image = t
                        .content
                        .iter()
                        .any(|c| matches!(c, InputContent::Image(_)));
                    let output = if has_image && model.supports_images() {
                        let mut parts = Vec::new();
                        if has_text {
                            parts.push(json!({"type":"input_text","text":text}));
                        }
                        parts.extend(
                            t.content
                                .into_iter()
                                .filter(|c| matches!(c, InputContent::Image(_)))
                                .map(converter_input),
                        );
                        Value::Array(parts)
                    } else {
                        Value::String(if has_text {
                            text
                        } else if has_image {
                            "(see attached image)".into()
                        } else {
                            "(no tool output)".into()
                        })
                    };
                    out.push(json!({"type":if self.fields.contains_key(&t.tool_name){"custom_tool_call_output"}else{"function_call_output"},"call_id":call,"output":output}));
                }
            }
            if !leading {
                msg_index += 1;
            }
        }
        if let Some(effort) = self.effort {
            push_config(effort, &mut out);
        }
        Ok((out, request_effort_baseline))
    }
}

pub(crate) struct ResponsesRequest {
    pub body: Value,
    pub grammar: HashMap<String, String>,
    pub provider_effort: Option<String>,
}

pub(crate) fn build_body(
    model: &Model,
    context: TranscriptContext,
    opts: &StreamOptions,
    d: Dialect,
) -> Result<ResponsesRequest> {
    let flag = |key, default| model.compat(key).as_bool().unwrap_or(default);
    let mid = flag("supportsMidConvoSystemMessages", false);
    let context = resolve_transcript(context, mid);
    let strict = flag("supportsStrictMode", d != Dialect::OpenAi);
    let grammar_tools = flag("supportsOpenAIGrammarTools", false);
    let tool_support = if mid && flag("supportsAdditionalTools", false) {
        TranscriptToolSupport::Changes
    } else if flag("supportsToolSearch", false) {
        TranscriptToolSupport::Additions
    } else {
        TranscriptToolSupport::None
    };
    let state = resolve_transcript_tools(context.messages(), tool_support);
    let mut fields = HashMap::new();
    for tool in get_declared_tools(context.messages()) {
        if let Some(g) = grammar(tool, grammar_tools)? {
            fields.insert(tool.name.clone(), g.property.to_owned());
        }
    }
    let sign_in = d == Dialect::OpenAi
        && model.provider == "openai"
        && model.base_url == "https://api.openai.com/v1"
        && opts
            .request
            .api_key
            .as_deref()
            .is_some_and(|k| !k.starts_with("sk-"));
    let level = opts
        .reasoning
        .map(|level| model.clamp_thinking_level(level))
        .unwrap_or(ThinkingLevel::Off);
    let mapped = model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&level));
    let requested = opts
        .specific
        .get("reasoningEffort")
        .and_then(Value::as_str)
        .or_else(|| (level != ThinkingLevel::Off).then(|| level.as_str()));
    let explicit_off = requested == Some("none")
        || requested.is_none() && d == Dialect::Codex && opts.reasoning.is_some();
    let effort = if explicit_off {
        if mapped == Some(&None) {
            None
        } else {
            mapped.and_then(Option::as_deref).or(Some("none"))
        }
    } else {
        requested.map(|r| mapped.and_then(Option::as_deref).unwrap_or(r))
    };
    let provider_effort =
        if d != Dialect::Azure && !sign_in && flag("supportsMidConvoEffort", false) {
            effort
        } else {
            None
        };
    let convert = Conversion {
        model,
        d,
        fields: &fields,
        tool_support,
        strict,
        grammar_tools,
        folded: state.folded_system_messages,
        effort: provider_effort,
        effort_baseline_initial: d == Dialect::Codex,
    };
    let tools = convert.tools(state.request_tools, false)?;
    let (messages, request_effort_baseline) = convert.convert(context)?;
    let id = if d == Dialect::Azure {
        azure_deployment(model, opts)
    } else {
        std::borrow::Cow::Borrowed(model.id.as_str())
    };
    let mut body = json!({"model":id,"input":messages,"stream":true,"store":false});
    let cache = retention(opts);
    if (d == Dialect::Azure || cache != CacheRetention::None)
        && model.compat("promptCacheKeyMode") != "disabled"
        && let Some(session) = &opts.session_id
    {
        body["prompt_cache_key"] = json!(session.chars().take(64).collect::<String>());
    }
    if d == Dialect::OpenAi && !sign_in {
        if flag("supportsExplicitPromptCacheMode", false) {
            if cache == CacheRetention::None {
                body["prompt_cache_options"] = json!({"mode":"explicit"});
            } else if cache == CacheRetention::Long && flag("supportsLongCacheRetention", true) {
                body["prompt_cache_options"] = json!({"ttl":"30m"});
            }
        } else if cache == CacheRetention::Long && flag("supportsLongCacheRetention", true) {
            body["prompt_cache_retention"] = json!("24h");
        }
    }
    if d == Dialect::Codex {
        let verbosity = opts
            .specific
            .get("textVerbosity")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("low");
        body["text"] = json!({"verbosity":verbosity});
        body["include"] = json!(["reasoning.encrypted_content"]);
        body["parallel_tool_calls"] = json!(true);
        body["tool_choice"] = json!("auto");
    }
    if !sign_in {
        if (d == Dialect::Azure || flag("supportsMaxOutputTokens", true))
            && let Some(max) = opts.max_tokens.filter(|max| *max > 0)
        {
            body["max_output_tokens"] = json!(max.max(16));
        }
        if let Some(temp) = opts.temperature {
            body["temperature"] = json!(temp);
        }
    }
    if d != Dialect::Azure
        && let Some(tier) = opts.specific.get("serviceTier")
    {
        body["service_tier"] = tier.clone();
    }
    if let Some(choice) = opts.specific.get("toolChoice") {
        body["tool_choice"] = choice.clone();
    } else if let Some(choice) = opts.tool_choice {
        body["tool_choice"] = json!(choice);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    let summary = opts
        .specific
        .get("reasoningSummary")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    if model.reasoning || d == Dialect::Codex {
        if let Some(effort) = effort.or_else(|| summary.map(|_| "medium")) {
            let wire_effort = if d == Dialect::Codex {
                request_effort_baseline.as_deref().unwrap_or(effort)
            } else if provider_effort.is_some() {
                "medium"
            } else {
                effort
            };
            body["reasoning"] = json!({"effort":wire_effort,"summary":summary.unwrap_or("auto")});
            body["include"] = json!(["reasoning.encrypted_content"]);
        } else if model.reasoning && mapped != Some(&None) && model.provider != "github-copilot" {
            body["reasoning"] =
                json!({"effort":mapped.and_then(Option::as_deref).unwrap_or("none")});
        }
        if model.provider == "xai" {
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
    }
    if d != Dialect::Codex
        && let Some(params) = body.as_object_mut()
    {
        params.extend(resolve_sampling_params(
            model,
            level,
            opts.sampling_params.clone(),
        ));
    }
    Ok(ResponsesRequest {
        body,
        grammar: fields,
        provider_effort: provider_effort.map(str::to_owned),
    })
}

async fn run(
    model: &Model,
    context: TranscriptContext,
    opts: &StreamOptions,
    state: &mut ChatState,
) -> Result<StopReason> {
    let d = Dialect::from_model(model);
    let key = request_key(model, opts, d)?;
    let mut headers = ProviderHeaders::from([
        ("content-type".into(), Some("application/json".into())),
        ("accept".into(), Some("text/event-stream".into())),
    ]);
    let url = if d == Dialect::Azure {
        headers.insert("api-key".into(), Some(key.to_owned()));
        resolve_azure_url(model, opts)?
    } else {
        headers.insert("authorization".into(), Some(format!("Bearer {key}")));
        copilot_headers(model, &context, &mut headers);
        if retention(opts) != CacheRetention::None
            && let Some(session) = opts.session_id.as_deref().filter(|s| !s.is_empty())
        {
            let affinity = model.compat("sessionAffinityFormat").as_str().unwrap_or(
                if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
                    "openrouter"
                } else {
                    "openai"
                },
            );
            if affinity == "openrouter" {
                headers.insert("x-session-id".into(), Some(session.into()));
            } else {
                if affinity == "openai" {
                    headers.insert("session-id".into(), Some(session.into()));
                }
                headers.insert("x-client-request-id".into(), Some(session.into()));
            }
        }
        endpoint(&model.base_url, "responses")?
    };
    let request = build_body(model, context, opts, d)?;
    write_partial(&state.message).provider_thinking_level = request.provider_effort;
    let body = payload(request.body, model, &opts.request).await?;
    let label = if d == Dialect::Azure {
        "Azure OpenAI"
    } else if model.provider == "openai" {
        "OpenAI"
    } else {
        model.provider.as_str()
    };
    let response = send(
        HttpRequest {
            method: Method::POST,
            url,
            headers: request_headers(&model.base, &opts.request, headers)?,
            body: Some(serde_json::to_vec(&body)?),
            error_label: format!("{label} API error"),
            force_http1: false,
            default_retries: 0,
            no_retry_statuses: vec![],
        },
        model,
        &opts.request,
    )
    .await?;
    consume_sse(response, state, model, opts, d, &request.grammar).await
}

pub(crate) async fn consume_sse(
    response: Response,
    state: &mut ChatState,
    model: &Model,
    opts: &StreamOptions,
    d: Dialect,
    grammar: &HashMap<String, String>,
) -> Result<StopReason> {
    state.start();
    let mut reader = SseReader::new(response.bytes_stream());
    let mut decoder = ResponsesEvents::default();
    while let Some(event) = reader.next(&opts.request.signal).await? {
        if event.data.trim() == "[DONE]" {
            break;
        }
        if event.data.trim().is_empty() {
            continue;
        }
        let data: Value = serde_json::from_str(&event.data)?;
        if let Some(observer) = &opts.on_provider_stream_event {
            abortable(&opts.request.signal, observer(&data, model)).await?;
        }
        decoder.accept(&data, state, model, opts, d, grammar)?;
    }
    decoder.finish(state)
}

enum Slot {
    Text(usize),
    Thinking(usize),
    Function(usize),
    Custom {
        index: usize,
        field: String,
        input: JsonGrammarInput,
    },
}
impl Slot {
    fn index(&self) -> usize {
        match self {
            Self::Text(i)
            | Self::Thinking(i)
            | Self::Function(i)
            | Self::Custom { index: i, .. } => *i,
        }
    }
}
#[derive(Default)]
pub(crate) struct ResponsesEvents {
    slots: HashMap<u64, Slot>,
    reasoning: HashMap<String, usize>,
    finish: Option<Result<StopReason>>,
    pub response_id: Option<String>,
}
impl ResponsesEvents {
    fn create_slot(
        item: &Value,
        state: &mut ChatState,
        grammar: &HashMap<String, String>,
    ) -> Result<Option<Slot>> {
        Ok(Some(match item["type"].as_str() {
            Some("message") => Slot::Text(state.text_start(None)),
            Some("reasoning") => Slot::Thinking(state.thinking_start(ThinkingContent::default())),
            Some("function_call" | "custom_tool_call") => {
                let name = item["name"].as_str().unwrap_or_default();
                let id = format!(
                    "{}|{}",
                    item["call_id"].as_str().unwrap_or("undefined"),
                    item["id"].as_str().unwrap_or("undefined")
                );
                let mut tool = ToolCall {
                    id,
                    name: name.to_owned(),
                    namespace: item["namespace"].as_str().map(str::to_owned),
                    ..ToolCall::default()
                };
                if item["type"] == "custom_tool_call" {
                    let field = grammar.get(name).map(String::as_str).unwrap_or("input");
                    let initial = item["input"].as_str().unwrap_or_default();
                    tool.arguments.insert(field.to_owned(), json!(initial));
                    let index = state.tool_start(tool);
                    let mut input = JsonGrammarInput::default();
                    if !initial.is_empty() {
                        state.delta(index, input.delta(field, initial)?)?;
                    }
                    Slot::Custom {
                        index,
                        field: field.to_owned(),
                        input,
                    }
                } else {
                    let index = state.tool_start(tool);
                    if let Some(value) = item["arguments"].as_str().filter(|v| !v.is_empty()) {
                        state.delta(index, value.to_owned())?;
                    }
                    Slot::Function(index)
                }
            }
            _ => return Ok(None),
        }))
    }
    pub fn accept(
        &mut self,
        event: &Value,
        state: &mut ChatState,
        model: &Model,
        opts: &StreamOptions,
        d: Dialect,
        fields: &HashMap<String, String>,
    ) -> Result<()> {
        let kind = event["type"].as_str().unwrap_or_default();
        let index = event["output_index"].as_u64().unwrap_or(u64::MAX);
        let delta = || event["delta"].as_str().unwrap_or_default().to_owned();
        match kind {
            "response.created" => {
                self.response_id = event["response"]["id"].as_str().map(str::to_owned);
                write_partial(&state.message)
                    .response_id
                    .clone_from(&self.response_id);
            }
            "response.output_item.added" => {
                if let Some(slot) = Self::create_slot(&event["item"], state, fields)? {
                    self.slots.insert(index, slot);
                }
            }
            "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta"
            | "response.reasoning_summary_part.done" => {
                if let Some(Slot::Thinking(i)) = self.slots.get(&index) {
                    state.delta(
                        *i,
                        if kind.ends_with("part.done") {
                            "\n\n".into()
                        } else {
                            delta()
                        },
                    )?;
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(Slot::Text(i)) = self.slots.get(&index) {
                    state.delta(*i, delta())?;
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(Slot::Function(i)) = self.slots.get(&index) {
                    state.delta(*i, delta())?;
                }
            }
            "response.function_call_arguments.done" => {
                if let Some(Slot::Function(i)) = self.slots.get(&index)
                    && let Some(args) = event["arguments"].as_str()
                {
                    state.replace_tool_args(*i, args)?;
                }
            }
            "response.custom_tool_call_input.delta" | "response.custom_tool_call_input.done" => {
                if let Some(Slot::Custom {
                    index,
                    field,
                    input,
                }) = self.slots.get_mut(&index)
                {
                    let data = if kind.ends_with(".done") {
                        input.close(field, event["input"].as_str())?
                    } else {
                        input.delta(field, event["delta"].as_str().unwrap_or_default())?
                    };
                    state.delta(*index, data)?;
                }
            }
            "response.output_item.done" => {
                let item = &event["item"];
                let slot = match self.slots.remove(&index) {
                    Some(slot) => Some(slot),
                    None => Self::create_slot(item, state, fields)?,
                };
                if let Some(mut slot) = slot {
                    let index = slot.index();
                    match &mut slot {
                        Slot::Thinking(_) => {
                            let summary = item["summary"]
                                .as_array()
                                .map(|v| {
                                    v.iter()
                                        .filter_map(|p| p["text"].as_str())
                                        .collect::<Vec<_>>()
                                        .join("\n\n")
                                })
                                .filter(|s| !s.is_empty())
                                .or_else(|| {
                                    item["content"]
                                        .as_array()
                                        .map(|v| {
                                            v.iter()
                                                .filter_map(|p| p["text"].as_str())
                                                .collect::<Vec<_>>()
                                                .join("\n\n")
                                        })
                                        .filter(|s| !s.is_empty())
                                });
                            let signature = serde_json::to_string(item)?;
                            state.thinking_update(index, |block| {
                                if let Some(summary) = summary {
                                    block.thinking = summary;
                                }
                                block.thinking_signature = Some(signature);
                            });
                            if let Some(id) = item["id"].as_str() {
                                self.reasoning.insert(id.to_owned(), index);
                            }
                        }
                        Slot::Text(_) => {
                            let text = item["content"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|v| {
                                            v["text"].as_str().or_else(|| v["refusal"].as_str())
                                        })
                                        .collect::<String>()
                                })
                                .unwrap_or_default();
                            let mut signature = json!({"v":1});
                            if let Some(id) = item.get("id") {
                                signature["id"] = id.clone();
                            }
                            if let Some(phase) = item.get("phase").filter(|p| !p.is_null()) {
                                signature["phase"] = phase.clone();
                            }
                            let signature = signature.to_string();
                            state.text_update(index, |block| {
                                block.text = text;
                                block.text_signature = Some(signature);
                            });
                        }
                        Slot::Function(_) => {
                            if let Some(args) = item["arguments"].as_str().filter(|a| !a.is_empty())
                            {
                                state.replace_tool_args(index, args)?;
                            }
                        }
                        Slot::Custom { field, input, .. } => {
                            state.delta(index, input.close(field, item["input"].as_str())?)?
                        }
                    }
                    if let Some(ns) = item["namespace"].as_str() {
                        state.tool_update(index, |t| t.namespace = Some(ns.to_owned()));
                    }
                    state.end(index);
                }
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                let response = &event["response"];
                let items = response["output"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                for item in items {
                    if item["type"] == "reasoning"
                        && let Some(encrypted) = item
                            .get("encrypted_content")
                            .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
                        && let Some(i) = item["id"].as_str().and_then(|id| self.reasoning.get(id))
                    {
                        state.thinking_update(*i, |t| {
                            if let Some(mut signature) = t
                                .thinking_signature
                                .as_deref()
                                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                                && signature["encrypted_content"].is_null()
                            {
                                signature["encrypted_content"] = encrypted.clone();
                                t.thinking_signature = Some(signature.to_string());
                            }
                        });
                    }
                }
                if let Some(id) = response["id"].as_str() {
                    self.response_id = Some(id.to_owned());
                }
                let status = response["status"].as_str();
                let incomplete = response["incomplete_details"]["reason"].as_str();
                let raw = incomplete
                    .map(|r| format!("{}.{r}", status.unwrap_or("undefined")))
                    .or_else(|| status.map(str::to_owned));
                let result = match status {
                    None | Some("completed" | "in_progress" | "queued") => {
                        if state.has_tool_calls() {
                            Ok(StopReason::ToolUse)
                        } else {
                            Ok(StopReason::Stop)
                        }
                    }
                    Some("incomplete") => {
                        if incomplete == Some("max_output_tokens") {
                            Ok(StopReason::Length)
                        } else {
                            Err(Error::Protocol(
                                incomplete
                                    .map(|r| format!("Response incomplete: {r}"))
                                    .unwrap_or_else(|| {
                                        "Response incomplete without a provider reason".into()
                                    }),
                            ))
                        }
                    }
                    Some("failed" | "cancelled") => {
                        Err(Error::Protocol("An unknown error occurred".into()))
                    }
                    Some(status) => {
                        Err(Error::Protocol(format!("Unhandled stop reason: {status}")))
                    }
                };
                let mut message = write_partial(&state.message);
                message.response_id.clone_from(&self.response_id);
                message.raw_stop_reason = raw;
                if let Some(usage) = response.get("usage").filter(|v| !v.is_null()) {
                    message.usage = calculate_usage(usage);
                }
                message.usage.calculate_cost(&model.cost);
                if d.supports_pricing() {
                    let provided = response["service_tier"].as_str();
                    let wanted = opts.specific.get("serviceTier").and_then(Value::as_str);
                    let tier = if d == Dialect::Codex
                        && provided == Some("default")
                        && matches!(wanted, Some("flex" | "priority"))
                    {
                        wanted
                    } else {
                        provided.or(wanted)
                    };
                    service_tier(&mut message.usage, &model.id, tier, d);
                }
                self.finish = Some(result);
            }
            "response.failed" => {
                let response = &event["response"];
                write_partial(&state.message).raw_stop_reason =
                    response["status"].as_str().map(str::to_owned);
                let error = &response["error"];
                let message = if error.is_object() {
                    format!(
                        "{}: {}",
                        error["code"].as_str().unwrap_or("unknown"),
                        error["message"].as_str().unwrap_or("no message")
                    )
                } else if let Some(reason) = response["incomplete_details"]["reason"].as_str() {
                    format!("incomplete: {reason}")
                } else {
                    "Unknown error (no error details in response)".into()
                };
                return Err(Error::Protocol(message));
            }
            "error" => {
                return Err(Error::Protocol(format!(
                    "Error Code {}: {}",
                    event["code"].as_str().unwrap_or("undefined"),
                    event["message"].as_str().unwrap_or("undefined")
                )));
            }
            _ => {}
        }
        Ok(())
    }
    pub fn terminal(&self) -> bool {
        self.finish.is_some()
    }
    pub fn finish(&mut self, state: &ChatState) -> Result<StopReason> {
        let reason = self.finish.take().ok_or_else(|| {
            Error::Protocol("OpenAI Responses stream ended before a terminal response event".into())
        })??;
        if reason == StopReason::ToolUse {
            let pending = self
                .slots
                .values()
                .filter(|s| matches!(s, Slot::Function(_) | Slot::Custom { .. }))
                .map(Slot::index)
                .min();
            if let Some(index) = pending
                && let Some(AssistantContent::ToolCall(call)) =
                    read_partial(&state.message).content.get(index)
            {
                return Err(Error::Protocol(format!(
                    "OpenAI Responses stream completed with an unfinished tool call: {} ({})",
                    call.name, call.id
                )));
            }
        }
        Ok(reason)
    }
}
fn calculate_usage(value: &Value) -> Usage {
    let details = &value["input_tokens_details"];
    let cache_read = details["cached_tokens"].as_f64().unwrap_or(0.0);
    let cache_write = details["cache_write_tokens"].as_f64().unwrap_or(0.0);
    Usage {
        input: (value["input_tokens"].as_f64().unwrap_or(0.0) - cache_read - cache_write).max(0.0),
        output: value["output_tokens"].as_f64().unwrap_or(0.0),
        cache_read,
        cache_write,
        total_tokens: value["total_tokens"].as_f64().unwrap_or(0.0),
        reasoning: Some(
            value["output_tokens_details"]["reasoning_tokens"]
                .as_f64()
                .unwrap_or(0.0),
        ),
        ..Usage::default()
    }
}
fn service_tier(usage: &mut Usage, model: &str, tier: Option<&str>, d: Dialect) {
    let multiplier = match tier {
        Some("flex") => 0.5,
        Some("priority") => {
            if model == "gpt-5.5" {
                2.5
            } else {
                2.0
            }
        }
        Some("fast") if d != Dialect::Codex => {
            if model == "gpt-5.5" {
                2.5
            } else {
                2.0
            }
        }
        _ => return,
    };
    let c = &mut usage.cost;
    c.input *= multiplier;
    c.output *= multiplier;
    c.cache_read *= multiplier;
    c.cache_write *= multiplier;
    c.total = c.input + c.output + c.cache_read + c.cache_write;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::{Map, Value, json};

    use super::replay_assistant;
    use crate::model::{Api, BaseModel, Model, ModelCost, ModelInputModality};
    use crate::types::{AssistantContent, AssistantMessage, StopReason, ThinkingContent};

    fn test_model(compat: Option<Map<String, Value>>) -> Model {
        Model {
            base: BaseModel {
                id: "local-reasoner".into(),
                name: "Local Reasoner".into(),
                api: Api::OpenAiResponses,
                provider: "local".into(),
                base_url: "http://127.0.0.1:8080/v1".into(),
                input: vec![ModelInputModality::Text],
                input_limits: None,
                cost: ModelCost::default(),
                headers: None,
            },
            kind: None,
            reasoning: true,
            thinking_level_map: None,
            prompt_cache: None,
            context_window: 32_768,
            max_tokens: 4_096,
            sampling_params: None,
            sampling_params_by_thinking_level: None,
            compat,
        }
    }

    fn assistant_with_unsigned_thinking(model: &Model) -> AssistantMessage {
        let mut msg = AssistantMessage::pending(model);
        msg.stop_reason = StopReason::Stop;
        msg.content
            .push(AssistantContent::Thinking(ThinkingContent {
                thinking: "step 1: check prefix cache".into(),
                ..ThinkingContent::default()
            }));
        msg
    }

    #[test]
    fn replay_assistant_emits_plain_reasoning_when_compat_flag_enabled() {
        let mut compat = Map::new();
        compat.insert(
            "requiresReasoningContentOnAssistantMessages".into(),
            Value::Bool(true),
        );
        let model = test_model(Some(compat));
        let parts = replay_assistant(
            &assistant_with_unsigned_thinking(&model),
            &model,
            &HashMap::new(),
            0,
        )
        .expect("replay should succeed");
        assert_eq!(
            parts,
            vec![json!({
                "type": "reasoning",
                "id": "rs_pi_0",
                "summary": [],
                "content": [{"type": "reasoning_text", "text": "step 1: check prefix cache"}],
            })]
        );
    }

    #[test]
    fn replay_assistant_omits_unsigned_thinking_by_default() {
        let model = test_model(None);
        let parts = replay_assistant(
            &assistant_with_unsigned_thinking(&model),
            &model,
            &HashMap::new(),
            0,
        )
        .expect("replay should succeed");
        assert!(parts.is_empty());
    }
}

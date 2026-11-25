use std::sync::LazyLock;
use std::time::SystemTime;

use aws_config::{BehaviorVersion, Region};
use aws_credential_types::{Credentials, provider::ProvideCredentials};
use aws_sigv4::{
    http_request::{SignableBody, SignableRequest, SigningParams, SigningSettings, sign},
    sign::v4,
};
use aws_smithy_eventstream::frame::{DecodedFrame, MessageFrameDecoder};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use bytes::BytesMut;
use futures_util::StreamExt;
use indexmap::IndexMap;
use regex::Regex;
use reqwest::{
    Method, StatusCode,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::{Map, Value, json};
use url::Url;

use crate::anthropic::{exceeded_strict, mark_over_limit, strict_limit_error};
use crate::chat_state::ChatState;
use crate::constrained::strict_parameters;
use crate::error::{Error, Result};
use crate::event_stream::{AssistantMessageEventStream, write_partial};
use crate::http::{HttpRequest, abortable, payload, request_headers, send};
use crate::model::{CacheRetention, Model, ThinkingLevel};
use crate::openai_completions::retention;
use crate::proxy::resolve_http_proxy_url_for_target;
use crate::request::{ProviderHeaders, StreamOptions, ToolChoice, provider_env_value};
use crate::simple_options::{
    adjust_max_tokens_for_thinking, build_base_options, clamp_max_tokens_to_context,
};
use crate::transcript::{
    get_current_tools, get_initial_system_message, resolve_transcript, system_message_text,
};
use crate::transform::{split_error_tool_result_images, transform_messages};
use crate::types::{
    AssistantContent, ConstrainedSampling, ConstrainedSamplingConfig, ImageContent, InputContent,
    Message, StopReason, StrictPreference, StringOrBlocks, ThinkingContent, ToolCall,
    TranscriptContext, unix_millis,
};

const REDACTED: &str = "[Reasoning redacted]";
const EMPTY: &str = "<empty>";
const BINDING: &str = "thinking-binding-controls-2026-08-01";
const MODELS: &[&str] = &[
    "opus-4-7", "opus-4-8", "opus-5", "sonnet-5", "haiku-5", "fable-5",
];
// INFALLIBLE: static Bedrock endpoint and inference ARN patterns.
static ARN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^arn:aws(?:-[a-z0-9-]+)?:bedrock:([a-z0-9-]+):")
        .expect("valid Bedrock inference-profile ARN pattern")
});
static HOST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^bedrock-runtime(?:-fips)?\.([a-z0-9-]+)\.amazonaws\.com(?:\.cn)?$")
        .expect("valid Bedrock runtime endpoint pattern")
});

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
) -> AssistantMessageEventStream {
    let mut opts = build_base_options(&model, &context, opts);
    let names = candidates(&model);
    if let Some(level) = opts.reasoning.filter(|l| *l != ThinkingLevel::Off)
        && claude(&model)
        && !is_adaptive(&names)
    {
        let (tokens, budget) = adjust_max_tokens_for_thinking(
            opts.max_tokens,
            model.max_tokens,
            level,
            &opts.thinking_budgets,
        );
        let max = clamp_max_tokens_to_context(&model, &context, tokens);
        opts.max_tokens = Some(max);
        opts.thinking_budgets
            .insert(budget_level(level), budget.min(max.saturating_sub(1024)));
    }
    stream(model, context, opts)
}
fn candidates(model: &Model) -> [String; 2] {
    let normalize = |s: &str| {
        let mut out = String::with_capacity(s.len());
        let mut dash = false;
        for c in s.chars() {
            if c.is_whitespace() || matches!(c, '_' | '.' | ':') {
                if !dash {
                    out.push('-');
                }
                dash = true;
            } else {
                out.extend(c.to_lowercase());
                dash = false;
            }
        }
        out
    };
    [normalize(&model.id), normalize(&model.name)]
}
fn matches(candidates: &[String], patterns: &[&str]) -> bool {
    candidates
        .iter()
        .any(|c| patterns.iter().any(|p| c.contains(p)))
}
fn claude(model: &Model) -> bool {
    let id = model.id.to_ascii_lowercase();
    id.contains("anthropic.claude")
        || id.contains("anthropic/claude")
        || model.name.to_ascii_lowercase().contains("claude")
}
fn is_adaptive(candidates: &[String]) -> bool {
    matches(candidates, MODELS) || matches(candidates, &["opus-4-6", "sonnet-4-6"])
}
fn budget_level(level: ThinkingLevel) -> ThinkingLevel {
    match level {
        ThinkingLevel::Xhigh | ThinkingLevel::Max => ThinkingLevel::High,
        _ => level,
    }
}
fn effort(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low | ThinkingLevel::Off => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}
fn string<'a>(opts: &'a StreamOptions, key: &str) -> Option<&'a str> {
    opts.specific
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}
fn env<'a>(opts: &'a StreamOptions, key: &str) -> Option<std::borrow::Cow<'a, str>> {
    provider_env_value(key, &opts.request.env)
}
fn cache_point(model: &Model, opts: &StreamOptions, names: &[String]) -> Option<Value> {
    let support = if matches(names, &["claude"]) {
        matches(
            names,
            &[
                "fable-5",
                "opus-5",
                "sonnet-5",
                "haiku-5",
                "-4-",
                "claude-3-7-sonnet",
                "claude-3-5-haiku",
            ],
        )
    } else {
        env(opts, "AWS_BEDROCK_FORCE_CACHE").as_deref() == Some("1")
    };
    match retention(opts) {
        CacheRetention::None => None,
        CacheRetention::Long if support => {
            Some(json!({"cachePoint":{"type":"default","ttl":"1h"}}))
        }
        _ if support => Some(json!({"cachePoint":{"type":"default"}})),
        _ => {
            let _ = model;
            None
        }
    }
}
fn normalize_id(id: &str) -> String {
    id.encode_utf16()
        .take(64)
        .map(|ch| {
            let c = char::from_u32(ch as u32).unwrap_or('_');
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}
fn decode64(value: &str) -> std::result::Result<Vec<u8>, base64::DecodeError> {
    if value.bytes().any(|b| b.is_ascii_whitespace()) {
        let raw = value
            .bytes()
            .filter(|b| !b.is_ascii_whitespace())
            .collect::<Vec<_>>();
        STANDARD
            .decode(&raw)
            .or_else(|_| STANDARD_NO_PAD.decode(raw))
    } else {
        STANDARD
            .decode(value)
            .or_else(|_| STANDARD_NO_PAD.decode(value))
    }
}
fn image(i: &ImageContent) -> Result<Value> {
    let format = match i.mime_type.as_str() {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        raw => return Err(Error::Protocol(format!("Unknown image type: {raw}"))),
    };
    let bytes = decode64(&i.data).map_err(|_| Error::Protocol("Invalid base64 image".into()))?;
    Ok(json!({"image":{"format":format,"source":{"bytes":STANDARD.encode(bytes)}}}))
}
fn nonblank(text: &str) -> Option<Value> {
    (!text.trim().is_empty()).then(|| json!({"text":text}))
}
fn sanitize_document(value: &mut Value) {
    match value {
        Value::Object(o) => {
            o.shift_remove("");
            o.values_mut().for_each(sanitize_document);
        }
        Value::Array(a) => a.iter_mut().for_each(sanitize_document),
        _ => {}
    }
}
fn parts(input: &[InputContent]) -> Result<Vec<Value>> {
    let mut parts = Vec::new();
    for item in input {
        match item {
            InputContent::Image(i) => parts.push(image(i)?),
            InputContent::Text(t) => parts.extend(nonblank(&t.text)),
        }
    }
    if parts.is_empty() {
        parts.push(json!({"text":EMPTY}));
    }
    Ok(parts)
}
fn messages(
    model: &Model,
    messages: Vec<Message>,
    point: Option<&Value>,
    claude: bool,
) -> Result<Vec<Value>> {
    let mut messages = messages;
    if matches!(messages.first(), Some(Message::System(_))) {
        messages.remove(0);
    }
    let messages = transform_messages(messages, model, |id, _, _| Some(normalize_id(id)));
    let mut result = Vec::with_capacity(messages.len());
    let mut iter = messages.into_iter().peekable();
    while let Some(m) = iter.next() {
        match m {
            Message::System(_) => {}
            Message::User(u) => {
                let content = match u.content {
                    StringOrBlocks::String(s) => {
                        vec![nonblank(&s).unwrap_or_else(|| json!({"text":EMPTY}))]
                    }
                    StringOrBlocks::Blocks(b) => parts(&b)?,
                };
                result.push(json!({"role":"user","content":content}));
            }
            Message::Assistant(a) => {
                let mut content = Vec::new();
                for block in a.content {
                    match block {
                        AssistantContent::Text(t) => content.extend(nonblank(&t.text)),
                        AssistantContent::Thinking(t) if t.redacted == Some(true) => {
                            if let Some(replay) = t
                                .thinking_signature
                                .and_then(|s| decode64(&s).ok())
                                .filter(|b| !b.is_empty())
                            {
                                content.push(json!({"reasoningContent":{"redactedContent":STANDARD.encode(replay)}}));
                            }
                        }
                        AssistantContent::Thinking(t) if !t.thinking.trim().is_empty() => {
                            let signature = t.thinking_signature.filter(|s| !s.trim().is_empty());
                            if claude && signature.is_none() {
                                content.push(json!({"text":t.thinking}));
                            } else {
                                let mut reasoning = json!({"text":t.thinking});
                                if claude && let Some(s) = signature {
                                    reasoning["signature"] = json!(s);
                                }
                                content
                                    .push(json!({"reasoningContent":{"reasoningText":reasoning}}));
                            }
                        }
                        AssistantContent::ToolCall(t) => {
                            let mut input = Value::Object(t.arguments);
                            sanitize_document(&mut input);
                            content.push(
                                json!({"toolUse":{"toolUseId":t.id,"name":t.name,"input":input}}),
                            );
                        }
                        _ => {}
                    }
                }
                if !content.is_empty() {
                    result.push(json!({"role":"assistant","content":content}));
                }
            }
            Message::ToolResult(mut t) => {
                let mut content = Vec::new();
                let mut afterwards = Vec::new();
                loop {
                    let items = if claude && let Some(split) = split_error_tool_result_images(&t) {
                        afterwards.push(json!({"text":split.caption}));
                        for i in split.images {
                            afterwards.push(image(&i)?);
                        }
                        let mut text = split
                            .content
                            .iter()
                            .filter_map(|t| nonblank(&t.text))
                            .collect::<Vec<_>>();
                        if text.is_empty() {
                            text.push(json!({"text":EMPTY}));
                        }
                        text
                    } else {
                        parts(&t.content)?
                    };
                    content.push(json!({"toolResult":{"toolUseId":t.tool_call_id,"content":items,"status":if t.is_error{"error"}else{"success"}}}));
                    if matches!(iter.peek(), Some(Message::ToolResult(_)))
                        && let Some(Message::ToolResult(next)) = iter.next()
                    {
                        t = next;
                    } else {
                        break;
                    }
                }
                content.append(&mut afterwards);
                result.push(json!({"role":"user","content":content}));
            }
        }
    }
    if let Some(point) = point
        && let Some(last) = result.last_mut().filter(|m| m["role"] == "user")
        && let Some(a) = last["content"].as_array_mut()
    {
        a.push(point.clone());
    }
    Ok(result)
}
fn configured_region(opts: &StreamOptions) -> Option<String> {
    string(opts, "region").map(str::to_owned).or_else(|| {
        env(opts, "AWS_REGION")
            .or_else(|| env(opts, "AWS_DEFAULT_REGION"))
            .map(std::borrow::Cow::into_owned)
    })
}
fn additional(
    model: &Model,
    opts: &StreamOptions,
    names: &[String],
    claude: bool,
) -> Option<Value> {
    let level = opts
        .reasoning
        .filter(|l| *l != ThinkingLevel::Off && model.reasoning)?;
    let mapped = model
        .thinking_level_map
        .as_ref()
        .and_then(|m| m.get(&level))
        .and_then(Option::as_deref);
    if !claude {
        if matches(names, &["gpt-oss"]) {
            return Some(json!({"reasoning_effort":effort(budget_level(level))}));
        }
        if matches(names, &["gpt-"]) {
            return Some(json!({"reasoning":{"effort":mapped.unwrap_or_else(||effort(level))}}));
        }
        return None;
    }
    let gov = configured_region(opts)
        .is_some_and(|s| s.to_ascii_lowercase().starts_with("us-gov-"))
        || model.id.starts_with("arn:aws-us-gov:")
        || model.id.starts_with("us-gov.");
    let adaptive = is_adaptive(names);
    let binding = !gov && matches(names, MODELS);
    let mut thinking = if adaptive {
        json!({"type":"adaptive"})
    } else {
        let key = budget_level(level);
        let default = match key {
            ThinkingLevel::Minimal => 1024,
            ThinkingLevel::Low => 2048,
            ThinkingLevel::Medium => 8192,
            _ => 16384,
        };
        json!({"type":"enabled","budget_tokens":opts.thinking_budgets.get(&key).copied().unwrap_or(default)})
    };
    if !gov {
        thinking["display"] = json!(string(opts, "thinkingDisplay").unwrap_or("summarized"));
    }
    if binding {
        thinking["block_binding"] = json!({"prefix_mismatch_behavior":"drop_block"});
    }
    let mut additional = json!({"thinking":thinking});
    if adaptive {
        let level = if level == ThinkingLevel::Xhigh && matches(names, MODELS) {
            "xhigh"
        } else {
            mapped.unwrap_or_else(|| effort(budget_level(level)))
        };
        additional["output_config"] = json!({"effort":level});
        if binding {
            additional["anthropic_beta"] = json!([BINDING]);
        }
    } else if opts.specific.get("interleavedThinking") != Some(&Value::Bool(false)) {
        additional["anthropic_beta"] = json!(["interleaved-thinking-2025-05-14"]);
    }
    Some(additional)
}
fn build(
    model: &Model,
    ctx: TranscriptContext,
    opts: &StreamOptions,
    disable_preferred: bool,
) -> Result<(Value, bool)> {
    let context = resolve_transcript(ctx, false);
    let names = candidates(model);
    let claude = claude(model);
    let point = cache_point(model, opts, &names);
    let mut has_preferred = false;
    let mut config = json!({});
    if let Some(n) = opts
        .max_tokens
        .or_else(|| claude.then_some(model.max_tokens))
    {
        config["maxTokens"] = json!(n);
    }
    if let Some(n) = opts.temperature {
        config["temperature"] = json!(n);
    }
    let mut body = json!({"modelId":model.id,"inferenceConfig":config});
    if let Some(prompt) = get_initial_system_message(context.messages())
        .map(system_message_text)
        .filter(|s| !s.is_empty())
    {
        let mut blocks = vec![json!({"text":prompt})];
        if let Some(point) = &point {
            blocks.push(point.clone());
        }
        body["system"] = json!(blocks);
    }
    if opts.tool_choice != Some(ToolChoice::None)
        && opts.specific.get("toolChoice").and_then(Value::as_str) != Some("none")
    {
        let tools = get_current_tools(context.messages());
        let strict_model = model
            .compat("supportsStrictMode")
            .as_bool()
            .unwrap_or(false);
        let tools=tools.into_iter().map(|t|{
            let preferred=matches!(t.constrained_sampling,Some(ConstrainedSampling::Enabled(ConstrainedSamplingConfig::JsonSchema{strict:StrictPreference::Prefer})));
            let (schema,strict)=strict_parameters(t,strict_model&&!(preferred&&disable_preferred),|_,_|false)?;
            let mut spec=json!({"name":t.name,"description":t.description,"inputSchema":{"json":schema}});
            if strict==Some(true){spec["strict"]=json!(true);has_preferred|=preferred;}
            Ok(json!({"toolSpec":spec}))
        }).collect::<Result<Vec<_>>>()?;
        if !tools.is_empty() {
            body["toolConfig"] = json!({"tools":tools});
            let choice = opts.specific.get("toolChoice");
            if choice.and_then(Value::as_str) == Some("auto")
                || choice.is_none() && opts.tool_choice == Some(ToolChoice::Auto)
            {
                body["toolConfig"]["toolChoice"] = json!({"auto":{}});
            } else if choice.and_then(Value::as_str) == Some("any") {
                body["toolConfig"]["toolChoice"] = json!({"any":{}});
            } else if let Some(name) = choice
                .filter(|v| v["type"] == "tool")
                .and_then(|v| v["name"].as_str())
            {
                body["toolConfig"]["toolChoice"] = json!({"tool":{"name":name}});
            }
        }
    }
    if let Some(extra) = additional(model, opts, &names, claude) {
        body["additionalModelRequestFields"] = extra;
    }
    if let Some(metadata) = opts.specific.get("requestMetadata") {
        body["requestMetadata"] = metadata.clone();
    }
    body["messages"] = json!(messages(
        model,
        context.into_messages(),
        point.as_ref(),
        claude
    )?);
    Ok((body, has_preferred))
}
struct Target {
    url: Url,
    region: String,
    auth: Auth,
    force_http1: bool,
}
enum Auth {
    Bearer(String),
    Signed(Credentials),
}
fn default_endpoint(region: &str) -> String {
    format!(
        "https://bedrock-runtime.{region}.amazonaws.com{}",
        if region.starts_with("cn-") { ".cn" } else { "" }
    )
}
fn cloud_error(error: impl std::fmt::Display) -> Error {
    Error::Auth(format!("AWS Bedrock credentials: {error}"))
}
async fn target(model: &Model, opts: &StreamOptions) -> Result<Target> {
    let explicit = string(opts, "profile")
        .or_else(|| opts.request.env.get("AWS_PROFILE").map(String::as_str))
        .filter(|p| !p.is_empty());
    let profile = explicit
        .map(str::to_owned)
        .or_else(|| env(opts, "AWS_PROFILE").map(std::borrow::Cow::into_owned));
    let configured = configured_region(opts);
    let base = Url::parse(&model.base_url).ok();
    let endpoint_region = base
        .as_ref()
        .and_then(Url::host_str)
        .and_then(|host| HOST.captures(host))
        .and_then(|c| c.get(1))
        .map(|m| m.as_str());
    let keep_custom =
        base.is_some() && (endpoint_region.is_none() || configured.is_none() && profile.is_none());
    let region = ARN
        .captures(&model.id)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_owned())
        .or(configured)
        .or_else(|| {
            keep_custom
                .then(|| endpoint_region.map(str::to_owned))
                .flatten()
        })
        .or_else(|| profile.is_none().then(|| "us-east-1".into()));
    let skip = env(opts, "AWS_BEDROCK_SKIP_AUTH").as_deref() == Some("1");
    let bearer = if skip {
        None
    } else {
        string(opts, "bearerToken")
            .or(opts.request.api_key.as_deref())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| env(opts, "AWS_BEARER_TOKEN_BEDROCK").map(std::borrow::Cow::into_owned))
    };
    let static_creds = if skip {
        Some(Credentials::new(
            "dummy-access-key",
            "dummy-secret-key",
            None,
            None,
            "bedrock-skip-auth",
        ))
    } else if explicit.is_none()
        && let (Some(key), Some(secret)) = (
            env(opts, "AWS_ACCESS_KEY_ID"),
            env(opts, "AWS_SECRET_ACCESS_KEY"),
        )
    {
        Some(Credentials::new(
            key.into_owned(),
            secret.into_owned(),
            env(opts, "AWS_SESSION_TOKEN").map(std::borrow::Cow::into_owned),
            None,
            "pi-env",
        ))
    } else {
        None
    };
    let need_chain = region.is_none() || bearer.is_none() && static_creds.is_none();
    let config = if need_chain {
        let mut builder = aws_config::defaults(BehaviorVersion::latest());
        if let Some(profile) = profile {
            builder = builder.profile_name(profile);
        }
        if let Some(region) = &region {
            builder = builder.region(Region::new(region.clone()));
        }
        Some(abortable(&opts.request.signal, async { Ok(builder.load().await) }).await?)
    } else {
        None
    };
    let region = region
        .or_else(|| {
            config
                .as_ref()
                .and_then(|c| c.region())
                .map(|r| r.as_ref().to_owned())
        })
        .unwrap_or_else(|| "us-east-1".into());
    let url = if keep_custom {
        base.ok_or_else(|| Error::Protocol("Invalid Bedrock endpoint".into()))?
    } else {
        Url::parse(&default_endpoint(&region))?
    };
    let auth = if let Some(token) = bearer {
        Auth::Bearer(token)
    } else {
        Auth::Signed(if let Some(creds) = static_creds {
            creds
        } else {
            let provider = config
                .as_ref()
                .and_then(|c| c.credentials_provider())
                .ok_or_else(|| Error::Auth("No AWS credentials provider".into()))?;
            abortable(&opts.request.signal, async {
                provider.provide_credentials().await.map_err(cloud_error)
            })
            .await?
        })
    };
    let force_http1 = env(opts, "AWS_BEDROCK_FORCE_HTTP1").as_deref() == Some("1")
        || resolve_http_proxy_url_for_target(url.as_str(), &opts.request.env)?.is_some();
    Ok(Target {
        url,
        region,
        auth,
        force_http1,
    })
}
fn sign_headers(
    headers: &mut HeaderMap,
    url: &Url,
    body: &[u8],
    region: &str,
    credentials: &Credentials,
) -> Result<()> {
    let identity = credentials.clone().into();
    let params: SigningParams<'_> = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name("bedrock")
        .time(SystemTime::now())
        .settings(SigningSettings::default())
        .build()
        .map_err(cloud_error)?
        .into();
    let request = SignableRequest::new(
        "POST",
        url.as_str(),
        headers
            .iter()
            .filter_map(|(k, v)| Some((k.as_str(), v.to_str().ok()?))),
        SignableBody::Bytes(body),
    )
    .map_err(cloud_error)?;
    let (signed, _) = sign(request, &params).map_err(cloud_error)?.into_parts();
    for (key, value) in signed.headers() {
        let mut value = HeaderValue::from_str(value).map_err(cloud_error)?;
        if matches!(key, "authorization" | "x-amz-security-token") {
            value.set_sensitive(true);
        }
        headers.insert(
            HeaderName::from_bytes(key.as_bytes()).map_err(cloud_error)?,
            value,
        );
    }
    Ok(())
}
fn reserved(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization")
        || name.eq_ignore_ascii_case("host")
        || name
            .get(..6)
            .is_some_and(|s| s.eq_ignore_ascii_case("x-amz-"))
}
async fn request(
    model: &Model,
    opts: &StreamOptions,
    target: &Target,
    mut input: Value,
) -> Result<reqwest::Response> {
    let mut url = target.url.clone();
    let id = input
        .as_object_mut()
        .and_then(|b| b.shift_remove("modelId"))
        .and_then(|id| id.as_str().map(str::to_owned))
        .unwrap_or_else(|| model.id.clone());
    url.path_segments_mut()
        .map_err(|()| Error::Protocol("Bedrock endpoint cannot be a base URL".into()))?
        .pop_if_empty()
        .extend(["model", id.as_str(), "converse-stream"]);
    let mut headers = ProviderHeaders::from([
        ("content-type".into(), Some("application/json".into())),
        (
            "accept".into(),
            Some("application/vnd.amazon.eventstream".into()),
        ),
    ]);
    let mut request = opts.request.clone();
    request.headers.retain(|k, _| !reserved(k));
    if let Auth::Bearer(key) = &target.auth {
        headers.insert("authorization".into(), Some(format!("Bearer {key}")));
    }
    let mut headers = request_headers(&model.base, &request, headers)?;
    headers.remove("host");
    if let Auth::Bearer(key) = &target.auth {
        let mut token = HeaderValue::from_str(&format!("Bearer {key}")).map_err(cloud_error)?;
        token.set_sensitive(true);
        headers.insert("authorization", token);
    }
    let body = serde_json::to_vec(&input)?;
    if let Auth::Signed(creds) = &target.auth {
        headers.remove("authorization");
        sign_headers(&mut headers, &url, &body, &target.region, creds)?;
    }
    send(
        HttpRequest {
            method: Method::POST,
            url,
            headers,
            body: Some(body),
            error_label: "Bedrock API error".into(),
            force_http1: target.force_http1,
            default_retries: 2,
            no_retry_statuses: vec![],
        },
        model,
        &request,
    )
    .await
}
fn hint(message: &str) -> String {
    if message.to_ascii_lowercase().contains("data retention mode") {
        format!(
            "{message} See https://docs.aws.amazon.com/bedrock/latest/userguide/data-retention.html for supported data retention modes."
        )
    } else {
        message.to_owned()
    }
}
fn diagnostic(state: &ChatState, kind: &str, details: Value) {
    let d = json!({"type":kind,"timestamp":unix_millis(),"details":details});
    if let Value::Object(d) = d {
        write_partial(&state.message)
            .diagnostics
            .get_or_insert_with(Vec::new)
            .push(d);
    }
}
async fn run(
    state: &mut ChatState,
    model: &Model,
    ctx: TranscriptContext,
    opts: &StreamOptions,
) -> Result<StopReason> {
    let target = target(model, opts).await?;
    let mut no_preferred = exceeded_strict(opts.session_id.as_deref());
    let response = loop {
        let (input, preferred) = build(model, ctx.clone(), opts, no_preferred)?;
        let value = payload(input, model, &opts.request).await?;
        match request(model, opts, &target, value).await {
            Ok(response) => break response,
            Err(Error::Http {
                status: StatusCode::BAD_REQUEST,
                message,
                ..
            }) if preferred && !no_preferred && strict_limit_error(&message) => {
                no_preferred = true;
                mark_over_limit(opts.session_id.as_deref());
                diagnostic(
                    state,
                    "bedrock_strict_tools_fallback",
                    json!({"reason":"strict_tool_limits_exceeded"}),
                );
            }
            Err(Error::Http {
                status,
                headers,
                message,
            }) => {
                let mut details = json!({"status":status.as_u16()});
                if let Some(r) = headers
                    .get("x-amzn-requestid")
                    .and_then(|s| s.to_str().ok())
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && s.len() <= 200)
                {
                    details["requestId"] = json!(r);
                }
                if let Some(kind) = headers
                    .get("x-amzn-errortype")
                    .and_then(|s| s.to_str().ok())
                    .and_then(|s| s.split(':').next())
                    .filter(|s| s.ends_with("Exception"))
                {
                    details["errorCode"] = json!(kind);
                }
                diagnostic(state, "bedrock_response_failure", details);
                return Err(Error::Protocol(hint(&message)));
            }
            Err(error) => return Err(error),
        }
    };
    let request_id = response
        .headers()
        .get("x-amzn-requestid")
        .and_then(|s| s.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 200)
        .map(str::to_owned);
    let result = consume(state, model, opts, response).await;
    if let Err(e) = &result
        && !matches!(e, Error::Aborted)
        && let Some(id) = request_id
    {
        diagnostic(state, "bedrock_response_failure", json!({"requestId":id}));
    }
    result
}
struct Block {
    index: usize,
    kind: &'static str,
    redacted: Vec<u8>,
}
fn flush(state: &mut ChatState, block: Block, end: bool) {
    if !block.redacted.is_empty() {
        state.thinking_update(block.index, |b| {
            b.thinking_signature = Some(STANDARD.encode(block.redacted))
        });
    }
    if end {
        state.end(block.index);
    }
}
fn stop(raw: Option<&str>) -> Result<StopReason> {
    match raw {
        Some("end_turn" | "stop_sequence") => Ok(StopReason::Stop),
        Some("max_tokens" | "model_context_window_exceeded") => Ok(StopReason::Length),
        Some("tool_use") => Ok(StopReason::ToolUse),
        Some(raw) => Err(Error::Protocol(format!("Provider stopped with: {raw}"))),
        None => Err(Error::Protocol("An unknown error occurred".into())),
    }
}
async fn consume(
    state: &mut ChatState,
    model: &Model,
    opts: &StreamOptions,
    response: reqwest::Response,
) -> Result<StopReason> {
    let mut stream = std::pin::pin!(response.bytes_stream());
    let mut body = BytesMut::new();
    let mut decoder = MessageFrameDecoder::new();
    let mut pending = false;
    let mut blocks = IndexMap::<u64, Block>::new();
    let mut finish = None;
    let result=async{
        while let Some(chunk)=abortable(&opts.request.signal,async{Ok(stream.next().await)}).await?{
            body.extend_from_slice(&chunk?);
            while !body.is_empty(){
                let bound=if !pending&&body.len()>=4{u32::from_be_bytes(body[..4].try_into().map_err(|_|Error::Protocol("Invalid Bedrock event size".into()))?)}else{0};
                if bound>24*1024*1024||body.len()>24*1024*1024{return Err(Error::Protocol("Bedrock EventStream frame is too large".into()));}
                let has_prelude = pending || body.len() >= 12;
                let frame=match decoder.decode_frame(&mut body).map_err(|e|Error::Protocol(format!("Invalid Bedrock EventStream frame: {e}")))?{
                    DecodedFrame::Incomplete=>{pending=has_prelude;break;}
                    DecodedFrame::Complete(frame)=>{pending=false;frame}
                };
                let header=|key|frame.headers().iter().find(|h|h.name().as_str()==key).and_then(|h|h.value().as_string().ok()).map(|v|v.as_str());
                let raw:Value=serde_json::from_slice(frame.payload())?;
                let kind=header(":event-type").or_else(||header(":exception-type")).or_else(||header(":error-code")).unwrap_or_default();
                if let Some(watch)=&opts.on_provider_stream_event{
                    let mut event=Map::new();event.insert(kind.to_owned(),raw.clone());let event=Value::Object(event);abortable(&opts.request.signal,watch(&event,model)).await?;
                }
                if matches!(header(":message-type"),Some("exception"|"error")){
                    let message=raw["message"].as_str().or_else(||header(":error-message")).unwrap_or(kind);
                    let prefix=match kind{"internalServerException"=>"Internal server error","modelStreamErrorException"=>"Model stream error","validationException"=>"Validation error","throttlingException"=>"Throttling error","serviceUnavailableException"=>"Service unavailable",_=>kind};
                    return Err(Error::Protocol(hint(&format!("{prefix}: {message}"))));
                }
                let wire_index=raw["contentBlockIndex"].as_u64().unwrap_or(u64::MAX);
                match kind{
                    "messageStart"=>{if raw["role"]!="assistant"{return Err(Error::Protocol("Unexpected assistant message start but got user message start instead".into()));}state.start();}
                    "contentBlockStart"=>if let Some(t)=raw.pointer("/start/toolUse").filter(|t|t.is_object()){
                        let i=state.tool_start(ToolCall{id:t["toolUseId"].as_str().unwrap_or_default().into(),name:t["name"].as_str().unwrap_or_default().into(),..ToolCall::default()});
                        blocks.insert(wire_index,Block{index:i,kind:"tool",redacted:vec![]});
                    },
                    "contentBlockDelta"=>{
                        let d=&raw["delta"];
                        if let Some(text)=d["text"].as_str(){
                            let block=blocks.entry(wire_index).or_insert_with(||Block{index:state.text_start(None),kind:"text",redacted:vec![]});
                            if block.kind=="text"{state.delta(block.index,text.into())?;}
                        }else if let Some(tool)=d.get("toolUse").filter(|t|t.is_object()){
                            if let Some(block)=blocks.get(&wire_index).filter(|b|b.kind=="tool"){state.delta(block.index,tool["input"].as_str().unwrap_or_default().into())?;}
                        }else if let Some(reasoning)=d.get("reasoningContent").filter(|r|r.is_object()){
                            let b=blocks.entry(wire_index).or_insert_with(||Block{index:state.thinking_start(ThinkingContent{thinking_signature:Some(String::new()),..ThinkingContent::default()}),kind:"thinking",redacted:vec![]});
                            if b.kind=="thinking"{
                                if let Some(text)=reasoning["text"].as_str().filter(|s|!s.is_empty()){state.delta(b.index,text.into())?;}
                                if b.redacted.is_empty()&&let Some(sig)=reasoning["signature"].as_str(){state.thinking_update(b.index,|p|p.thinking_signature.get_or_insert_with(String::new).push_str(sig));}
                                if let Some(data)=reasoning["redactedContent"].as_str().filter(|s|!s.is_empty()){
                                    if b.redacted.is_empty(){state.thinking_update(b.index,|p|{p.redacted=Some(true);p.thinking_signature=Some(String::new());});state.delta(b.index,REDACTED.into())?;}
                                    b.redacted.extend_from_slice(&decode64(data).map_err(|e|Error::Protocol(format!("Invalid Bedrock redacted reasoning: {e}")))?);
                                }
                            }
                        }
                    }
                    "contentBlockStop"=>{if let Some(block)=blocks.shift_remove(&wire_index){flush(state,block,true);}}
                    "messageStop"=>{
                        let reason=raw["stopReason"].as_str();write_partial(&state.message).raw_stop_reason=reason.map(str::to_owned);finish=Some(stop(reason));
                    }
                    "metadata"=>if let Some(usage)=raw.get("usage").filter(|u|u.is_object()){
                        let n=|key|usage[key].as_f64().unwrap_or_default();let mut msg=write_partial(&state.message);
                        msg.usage.input=n("inputTokens");msg.usage.output=n("outputTokens");msg.usage.cache_read=n("cacheReadInputTokens");msg.usage.cache_write=n("cacheWriteInputTokens");
                        msg.usage.cache_write_1h=usage["cacheDetails"].as_array().map(|details|details.iter().filter(|d|d["ttl"]=="1h").filter_map(|d|d["inputTokens"].as_f64()).sum());
                        msg.usage.total_tokens=usage["totalTokens"].as_f64().filter(|n|*n!=0.0).unwrap_or(msg.usage.input+msg.usage.output);msg.usage.calculate_cost(&model.cost);
                    },
                    _=>{}
                }
            }
        }
        if pending||!body.is_empty(){return Err(Error::Protocol("Truncated Bedrock EventStream frame".into()));}
        finish.ok_or_else(||Error::Protocol("Bedrock stream ended without a stop reason".into()))?
    }.await;
    for block in blocks.into_values() {
        flush(state, block, false);
    }
    result
}

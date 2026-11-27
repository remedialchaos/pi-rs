use std::collections::HashSet;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::{Engine, engine::general_purpose::STANDARD};
use google_cloud_auth::credentials::{self, CacheableResource, Credentials};
use regex::Regex;
use reqwest::Method;
use serde_json::{Map, Value, json};
use url::Url;

use crate::chat_state::ChatState;
use crate::constrained::strict_parameters;
use crate::error::{Error, Result};
use crate::event_stream::{AssistantMessageEventStream, write_partial};
use crate::http::{HttpRequest, abortable, endpoint, payload, request_headers, send};
use crate::model::{Api, Model, ModelInputModality, ThinkingLevel};
use crate::request::{ProviderHeaders, StreamOptions, provider_env_value};
use crate::simple_options::build_base_options;
use crate::sse::SseReader;
use crate::transcript::{
    get_current_tools, get_initial_system_message, input_text, resolve_transcript,
    system_message_text,
};
use crate::transform::transform_messages;
use crate::types::{
    AssistantContent, ImageContent, InputContent, Message, StopReason, StringOrBlocks,
    ThinkingContent, ToolCall, TranscriptContext, unix_millis,
};

static TOOL_COUNTER: AtomicU64 = AtomicU64::new(0);
// INFALLIBLE: these regex literals are tested by component tests.
static VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)gemini(?:-live)?-(\d+)").expect("valid Gemini version pattern")
});
// INFALLIBLE: these regex literals are tested by component tests.
static DISCRETE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"gemini-3(?:\.\d+)?-(?:pro|flash)|gemma-?4")
        .expect("valid Gemini discrete thinking pattern")
});
// INFALLIBLE: these regex literals are tested by component tests.
static API_VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^v\d+(?:beta\d*)?$").expect("valid Google API version pattern"));
const CLOUD_SCOPE: [&str; 1] = ["https://www.googleapis.com/auth/cloud-platform"];
fn major(id: &str) -> Option<u64> {
    VERSION.captures(id)?.get(1)?.as_str().parse().ok()
}
fn uses_level(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    DISCRETE.is_match(&id)
        || major(&id).is_some_and(|n| n >= 4)
        || matches!(
            id.as_str(),
            "gemini-flash-latest" | "gemini-flash-lite-latest"
        )
}
fn resolve_level(model: &Model, requested: ThinkingLevel) -> Result<&'static str> {
    let mapped = model
        .thinking_level_map
        .as_ref()
        .and_then(|m| m.get(&requested))
        .and_then(Option::as_deref);
    match mapped
        .unwrap_or_else(|| requested.as_str())
        .to_ascii_lowercase()
        .as_str()
    {
        "minimal" => Ok("MINIMAL"),
        "low" => Ok("LOW"),
        "medium" => Ok("MEDIUM"),
        "high" => Ok("HIGH"),
        "xhigh" => Ok("XHIGH"),
        _ => Err(Error::Protocol(format!(
            "Unsupported Google thinking level mapping for {}/{}: {} -> {}",
            model.provider,
            model.id,
            requested.as_str(),
            mapped.unwrap_or("undefined")
        ))),
    }
}
fn budget(model: &Model, level: &str, opts: &StreamOptions) -> i64 {
    let (index, key) = match level {
        "MINIMAL" => (0, ThinkingLevel::Minimal),
        "LOW" => (1, ThinkingLevel::Low),
        "MEDIUM" => (2, ThinkingLevel::Medium),
        _ => (3, ThinkingLevel::High),
    };
    if let Some(n) = opts.thinking_budgets.get(&key) {
        return i64::try_from(*n).unwrap_or(i64::MAX);
    }
    let id = &model.id;
    if id.contains("2.5-pro") {
        [128, 2048, 8192, 32768][index]
    } else if id.contains("2.5-flash-lite") {
        [512, 2048, 8192, 24576][index]
    } else if id.contains("2.5-flash") {
        [128, 2048, 8192, 24576][index]
    } else {
        -1
    }
}
fn api_key(opts: &StreamOptions, vertex: bool) -> Option<&str> {
    opts.request
        .api_key
        .as_deref()
        .map(|s| if vertex { s.trim() } else { s })
        .filter(|s| {
            !s.is_empty()
                && (!vertex
                    || *s != "gcp-vertex-credentials"
                        && !(s.starts_with('<') && s.ends_with('>') && s.len() > 2))
        })
}
fn key_error(model: &Model) -> Error {
    Error::Auth(format!("No API key for provider: {}", model.provider))
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
    if model.api != Api::GoogleVertex && api_key(&opts, false).is_none() {
        return Err(key_error(&model));
    }
    let requested = opts
        .reasoning
        .map(|r| model.clamp_thinking_level(r))
        .filter(|r| *r != ThinkingLevel::Off);
    let mut opts = build_base_options(&model, &context, opts);
    let thinking = if let Some(requested) = requested {
        let mapped = resolve_level(&model, requested)?;
        if uses_level(&model.id) {
            json!({"enabled":true,"level":mapped})
        } else {
            json!({"enabled":true,"budgetTokens":budget(&model,mapped,&opts)})
        }
    } else {
        json!({"enabled":false})
    };
    opts.specific.insert("thinking".into(), thinking);
    Ok(stream(model, context, opts))
}
fn thought_wire(
    text: String,
    signature: Option<String>,
    thought: bool,
    same: bool,
) -> Option<Value> {
    let signature = signature
        .filter(|s| same && !s.is_empty() && s.len() % 4 == 0 && STANDARD.decode(s).is_ok());
    if text.trim().is_empty() && signature.is_none() {
        return None;
    }
    let mut part = json!({"text":text});
    if thought && same {
        part["thought"] = Value::Bool(true);
    }
    if let Some(sig) = signature {
        part["thoughtSignature"] = json!(sig);
    }
    Some(part)
}
fn inline(i: &ImageContent) -> Value {
    json!({"inlineData":{"mimeType":i.mime_type,"data":i.data}})
}
fn normalized(id: &str) -> String {
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
fn messages(model: &Model, ctx: TranscriptContext) -> Vec<Value> {
    let major = major(&model.id);
    let use_ids = major.is_some_and(|m| m >= 3)
        || model.id.starts_with("claude-")
        || model.id.starts_with("gpt-oss-");
    let multimodal = major.is_none_or(|v| v >= 3);
    let mut raw = ctx.into_messages();
    if matches!(raw.first(), Some(Message::System(_))) {
        raw.remove(0);
    }
    let messages = transform_messages(raw, model, |id, _, _| use_ids.then(|| normalized(id)));
    let mut out = Vec::<Value>::with_capacity(messages.len());
    for message in messages {
        match message {
            Message::System(_) => {}
            Message::User(u) => {
                let parts = match u.content {
                    StringOrBlocks::String(s) => vec![json!({"text":s})],
                    StringOrBlocks::Blocks(a) => a
                        .into_iter()
                        .map(|p| match p {
                            InputContent::Text(t) => json!({"text":t.text}),
                            InputContent::Image(i) => inline(&i),
                        })
                        .collect(),
                };
                if !parts.is_empty() {
                    out.push(json!({"role":"user","parts":parts}));
                }
            }
            Message::Assistant(a) => {
                let same = a.provider == model.provider && a.model == model.id;
                let parts = a
                    .content
                    .into_iter()
                    .filter_map(|p| match p {
                        AssistantContent::Text(t) => {
                            thought_wire(t.text, t.text_signature, false, same)
                        }
                        AssistantContent::Thinking(t) => {
                            thought_wire(t.thinking, t.thinking_signature, true, same)
                        }
                        AssistantContent::ToolCall(t) => {
                            let mut call = json!({"name":t.name,"args":t.arguments});
                            if use_ids {
                                call["id"] = json!(t.id);
                            }
                            let mut part = json!({"functionCall":call});
                            if let Some(sig) = t.thought_signature.filter(|s| {
                                same && !s.is_empty()
                                    && s.len() % 4 == 0
                                    && STANDARD.decode(s).is_ok()
                            }) {
                                part["thoughtSignature"] = json!(sig);
                            }
                            Some(part)
                        }
                    })
                    .collect::<Vec<_>>();
                if !parts.is_empty() {
                    out.push(json!({"role":"model","parts":parts}));
                }
            }
            Message::ToolResult(result) => {
                let text = input_text(&result.content, "\n");
                let image_parts = if model.input.contains(&ModelInputModality::Image) {
                    result
                        .content
                        .iter()
                        .filter_map(|p| match p {
                            InputContent::Image(i) => Some(inline(i)),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                } else {
                    vec![]
                };
                let reply = if !text.trim().is_empty() {
                    text.as_str()
                } else if !image_parts.is_empty() {
                    "(see attached image)"
                } else {
                    ""
                };
                let answer = if result.is_error { "error" } else { "output" };
                let mut function = json!({"name":result.tool_name,"response":{(answer):reply}});
                if use_ids {
                    function["id"] = json!(result.tool_call_id);
                }
                let follow_images = if !image_parts.is_empty() && multimodal {
                    function["parts"] = json!(image_parts);
                    None
                } else {
                    Some(image_parts).filter(|p| !p.is_empty())
                };
                let block = json!({"functionResponse":function});
                if let Some(last) = out.last_mut().filter(|m| m["role"] == "user")
                    && let Some(p) = last
                        .get_mut("parts")
                        .and_then(Value::as_array_mut)
                        .filter(|p| p.iter().any(|b| b.get("functionResponse").is_some()))
                {
                    p.push(block);
                } else {
                    out.push(json!({"role":"user","parts":[block]}));
                }
                if let Some(mut images) = follow_images {
                    images.insert(0, json!({"text":"Tool result image:"}));
                    out.push(json!({"role":"user","parts":images}));
                }
            }
        }
    }
    out
}
fn rewrite_const(schema: &mut Value, by_name: bool) {
    match schema {
        Value::Array(a) => a.iter_mut().for_each(|p| rewrite_const(p, false)),
        Value::Object(entries) => {
            for (k, v) in entries.iter_mut() {
                if by_name
                    || matches!(
                        k.as_str(),
                        "additionalProperties"
                            | "unevaluatedProperties"
                            | "propertyNames"
                            | "items"
                            | "prefixItems"
                            | "additionalItems"
                            | "unevaluatedItems"
                            | "contains"
                            | "allOf"
                            | "anyOf"
                            | "oneOf"
                            | "not"
                            | "if"
                            | "then"
                            | "else"
                            | "contentSchema"
                    )
                {
                    rewrite_const(v, false);
                } else if matches!(
                    k.as_str(),
                    "properties"
                        | "patternProperties"
                        | "$defs"
                        | "definitions"
                        | "dependentSchemas"
                        | "dependencies"
                ) {
                    rewrite_const(v, true);
                }
            }
            if !by_name
                && !entries.contains_key("enum")
                && entries
                    .get("const")
                    .is_some_and(|v| v.is_string() || v.is_number())
                && let Some(value) = entries.shift_remove("const")
            {
                entries.insert("enum".into(), Value::Array(vec![value]));
            }
        }
        _ => {}
    }
}
fn build(model: &Model, ctx: TranscriptContext, opts: &StreamOptions) -> Result<Value> {
    let ctx = resolve_transcript(ctx, false);
    let tools = get_current_tools(ctx.messages());
    let prompt = get_initial_system_message(ctx.messages())
        .map(system_message_text)
        .filter(|s| !s.is_empty());
    let supported = model
        .compat("supportsStrictMode")
        .as_bool()
        .unwrap_or_else(|| major(&model.id).is_some_and(|v| v >= 3));
    let mut any_strict = false;
    let decl = tools
        .into_iter()
        .map(|t| {
            let (mut schema, strict) = strict_parameters(t, supported, |_, _| false)?;
            any_strict |= strict == Some(true);
            rewrite_const(schema.to_mut(), false);
            Ok(json!({"name":t.name,"description":t.description,"parametersJsonSchema":schema}))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut cfg = json!({"automaticContinuation":false});
    if let Some(prompt) = prompt {
        cfg["systemInstruction"] = json!(prompt);
    }
    if !decl.is_empty() {
        cfg["tools"] = json!([{"functionDeclarations":decl}]);
        let choice = opts
            .specific
            .get("toolChoice")
            .and_then(Value::as_str)
            .or_else(|| {
                opts.tool_choice.map(|c| match c {
                    crate::request::ToolChoice::Auto => "auto",
                    crate::request::ToolChoice::None => "none",
                })
            });
        let mode = match choice {
            Some("none") => Some("NONE"),
            Some("any") => Some("ANY"),
            _ if any_strict => Some("VALIDATED"),
            Some(_) => Some("AUTO"),
            _ => None,
        };
        if let Some(mode) = mode {
            cfg["toolConfig"] = json!({"functionCallingConfig":{"mode":mode}});
        }
    }
    if let Some(n) = opts.temperature {
        cfg["temperature"] = json!(n);
    }
    if let Some(n) = opts.max_tokens {
        cfg["maxOutputTokens"] = json!(n);
    }
    if model.reasoning
        && let Some(thinking) = opts.specific.get("thinking")
    {
        let value = if thinking["enabled"] == true {
            let mut value = json!({"includeThoughts":true});
            if let Some(level) = thinking["level"].as_str() {
                value["thinkingLevel"] = json!(level);
            } else if let Some(budget) = thinking.get("budgetTokens") {
                value["thinkingBudget"] = budget.clone();
            }
            value
        } else {
            let disabled = model.clamp_thinking_level(ThinkingLevel::Off);
            if uses_level(&model.id) && disabled != ThinkingLevel::Off {
                json!({"thinkingLevel":resolve_level(model,disabled)?})
            } else {
                json!({"thinkingBudget":0})
            }
        };
        cfg["thinkingConfig"] = value;
    }
    Ok(json!({"model":model.id,"contents":messages(model,ctx),"config":cfg}))
}
fn wire(mut input: Value) -> Value {
    let mut config = input["config"]
        .as_object_mut()
        .map(std::mem::take)
        .unwrap_or_default();
    let mut body = json!({"contents":input["contents"].take()});
    for field in [
        "tools",
        "toolConfig",
        "systemInstruction",
        "cachedContent",
        "labels",
        "safetySettings",
    ] {
        if let Some(mut value) = config.shift_remove(field) {
            if field == "systemInstruction"
                && let Some(text) = value.as_str()
            {
                value = json!({"parts":[{"text":text}],"role":"user"});
            }
            body[field] = value;
        }
    }
    for field in [
        "automaticContinuation",
        "abortSignal",
        "httpOptions",
        "automaticFunctionCalling",
    ] {
        config.shift_remove(field);
    }
    if !config.is_empty() {
        body["generationConfig"] = Value::Object(config);
    }
    body
}
fn project_location(opts: &StreamOptions) -> Result<(String, String)> {
    let configured = |key, env: &[&str]| {
        opts.specific
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                env.iter().find_map(|e| {
                    provider_env_value(e, &opts.request.env).map(std::borrow::Cow::into_owned)
                })
            })
    };
    let project=configured("project",&["GOOGLE_CLOUD_PROJECT","GCLOUD_PROJECT"]).ok_or_else(||Error::Auth("Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT or pass project in options.".into()))?;
    let location = configured("location", &["GOOGLE_CLOUD_LOCATION"]).ok_or_else(|| {
        Error::Auth(
            "Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION or pass location in options."
                .into(),
        )
    })?;
    Ok((project, location))
}
fn credentials_error(error: impl std::fmt::Display) -> Error {
    Error::Auth(format!("Google Application Default Credentials: {error}"))
}
async fn adc(opts: &StreamOptions) -> Result<ProviderHeaders> {
    let config = opts
        .request
        .env
        .get("GOOGLE_APPLICATION_CREDENTIALS")
        .filter(|s| !s.is_empty());
    let credentials: Credentials = if let Some(path) = config {
        let value: Value = serde_json::from_slice(&tokio::fs::read(path).await?)?;
        match value["type"].as_str() {
            Some("service_account") => credentials::service_account::Builder::new(value)
                .with_access_specifier(credentials::service_account::AccessSpecifier::from_scopes(
                    CLOUD_SCOPE,
                ))
                .build(),
            Some("authorized_user") => credentials::user_account::Builder::new(value)
                .with_scopes(CLOUD_SCOPE)
                .build(),
            Some("external_account") => credentials::external_account::Builder::new(value)
                .with_scopes(CLOUD_SCOPE)
                .build(),
            Some("external_account_authorized_user") => {
                credentials::external_account_authorized_user::Builder::new(value).build()
            }
            Some("impersonated_service_account") => credentials::impersonated::Builder::new(value)
                .with_scopes(CLOUD_SCOPE)
                .build(),
            _ => {
                return Err(Error::Auth(
                    "Google Application Default Credentials file has an unsupported type".into(),
                ));
            }
        }
        .map_err(credentials_error)?
    } else {
        credentials::Builder::default()
            .with_scopes(CLOUD_SCOPE)
            .build()
            .map_err(credentials_error)?
    };
    let headers = abortable(&opts.request.signal, async {
        credentials
            .headers(Default::default())
            .await
            .map_err(credentials_error)
    })
    .await?;
    if let CacheableResource::New { data, .. } = headers {
        Ok(data
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_owned(), Some(v.to_str().ok()?.to_owned()))))
            .collect())
    } else {
        Err(Error::Auth(
            "Google Application Default Credentials did not return headers".into(),
        ))
    }
}
fn location_host(location: &str) -> String {
    if location == "global" {
        "https://aiplatform.googleapis.com".into()
    } else {
        format!("https://{location}-aiplatform.googleapis.com")
    }
}
fn target(model: &Model, id: &str, project: Option<(&str, &str)>) -> Result<Url> {
    let vertex = model.api == Api::GoogleVertex;
    let custom = !model.base_url.trim().is_empty() && !model.base_url.contains("{location}");
    let (base, path) = if !vertex {
        let base = if custom {
            model.base_url.clone()
        } else {
            "https://generativelanguage.googleapis.com/v1beta".into()
        };
        let prefix = if id.contains('/') { "" } else { "models/" };
        (base, format!("{prefix}{id}:streamGenerateContent"))
    } else {
        let mut base = if custom {
            model.base_url.clone()
        } else {
            location_host(project.map_or("global", |(_, l)| l))
        };
        if !Url::parse(&base)?
            .path()
            .split('/')
            .any(|s| API_VERSION.is_match(s))
        {
            base = endpoint(&base, "v1")?.into();
        }
        let publisher_model = if id.starts_with("projects/") || id.starts_with("publishers/") {
            id.into()
        } else if let Some((publisher, name)) = id.split_once('/') {
            format!("publishers/{publisher}/models/{name}")
        } else {
            format!("publishers/google/models/{id}")
        };
        let path = if !custom
            && !publisher_model.starts_with("projects/")
            && let Some((p, l)) = project
        {
            format!("projects/{p}/locations/{l}/{publisher_model}:streamGenerateContent")
        } else {
            format!("{publisher_model}:streamGenerateContent")
        };
        (base, path)
    };
    let mut url = endpoint(&base, &path)?;
    url.query_pairs_mut().append_pair("alt", "sse");
    Ok(url)
}
fn format_prompt_block_error(provider_name: &str, feedback: &Value) -> String {
    let reason = feedback["blockReason"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown reason");
    let mut details = vec![format!("{provider_name} prompt blocked: {reason}")];
    if let Some(msg) = feedback["blockReasonMessage"]
        .as_str()
        .filter(|s| !s.is_empty())
    {
        details.push(msg.to_owned());
    }
    if let Some(ratings) = feedback["safetyRatings"].as_array() {
        let formatted: Vec<String> = ratings
            .iter()
            .filter_map(|rating| {
                let category = rating["category"].as_str().filter(|s| !s.is_empty())?;
                let prob = rating["probability"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|p| format!(" ({p})"))
                    .unwrap_or_default();
                Some(format!("{category}{prob}"))
            })
            .collect();
        if !formatted.is_empty() {
            let label = if formatted.len() == 1 {
                "Safety rating"
            } else {
                "Safety ratings"
            };
            details.push(format!("{label}: {}", formatted.join(", ")));
        }
    }
    details.join(". ")
}

async fn run(
    state: &mut ChatState,
    model: &Model,
    ctx: TranscriptContext,
    opts: &StreamOptions,
) -> Result<StopReason> {
    let vertex = model.api == Api::GoogleVertex;
    let key = api_key(opts, vertex);
    let (mut headers, project) = if let Some(key) = key {
        (
            ProviderHeaders::from([("x-goog-api-key".into(), Some(key.into()))]),
            None,
        )
    } else if vertex {
        let p = project_location(opts)?;
        (adc(opts).await?, Some(p))
    } else {
        return Err(key_error(model));
    };
    let params = payload(build(model, ctx, opts)?, model, &opts.request).await?;
    let id = params["model"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(&model.id);
    let url = target(
        model,
        id,
        project.as_ref().map(|(p, l)| (p.as_str(), l.as_str())),
    )?;
    headers.insert("content-type".into(), Some("application/json".into()));
    let response = send(
        HttpRequest {
            method: Method::POST,
            url,
            headers: request_headers(&model.base, &opts.request, headers)?,
            body: Some(serde_json::to_vec(&wire(params))?),
            error_label: "Google API error".into(),
            force_http1: false,
            default_retries: 0,
            no_retry_statuses: vec![],
        },
        model,
        &opts.request,
    )
    .await?;
    state.start();
    let provider_name = if vertex { "Google Vertex" } else { "Google" };
    let mut frames = SseReader::new(response.bytes_stream());
    let mut current = None;
    let mut seen = HashSet::<String>::new();
    let mut finish = None;
    let mut received_chunk_count: usize = 0;
    let mut prompt_block_error: Option<String> = None;
    while let Some(event) = frames.next(&opts.request.signal).await? {
        if matches!(event.data.trim(), "" | "[DONE]") {
            continue;
        }
        let chunk: Value = serde_json::from_str(&event.data)?;
        if let Some(watch) = &opts.on_provider_stream_event {
            abortable(&opts.request.signal, watch(&chunk, model)).await?;
        }
        received_chunk_count += 1;
        if prompt_block_error.is_none()
            && chunk["promptFeedback"]["blockReason"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        {
            prompt_block_error = Some(format_prompt_block_error(
                provider_name,
                &chunk["promptFeedback"],
            ));
        }
        let candidate = &chunk["candidates"][0];
        {
            let mut msg = write_partial(&state.message);
            if msg.response_id.is_none() {
                msg.response_id = chunk["responseId"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
            }
        }
        if let Some(parts) = candidate["content"]["parts"].as_array() {
            for part in parts {
                let signature = part["thoughtSignature"].as_str().filter(|s| !s.is_empty());
                if let Some(text) = part["text"].as_str() {
                    let thought = part["thought"] == true;
                    let idx = match current {
                        Some((i, was)) if was == thought => i,
                        previous => {
                            if let Some((i, _)) = previous {
                                state.end(i);
                            }
                            let i = if thought {
                                state.thinking_start(ThinkingContent::default())
                            } else {
                                state.text_start(None)
                            };
                            current = Some((i, thought));
                            i
                        }
                    };
                    if let Some(sig) = signature {
                        if thought {
                            state.thinking_update(idx, |r| r.thinking_signature = Some(sig.into()));
                        } else {
                            state.text_update(idx, |r| r.text_signature = Some(sig.into()));
                        }
                    }
                    state.delta(idx, text.into())?;
                }
                if let Some(function) = part.get("functionCall").filter(|p| p.is_object()) {
                    if let Some((i, _)) = current.take() {
                        state.end(i);
                    }
                    let name = function["name"].as_str().unwrap_or_default();
                    let id = if let Some(s) = function["id"]
                        .as_str()
                        .filter(|s| !s.is_empty() && !seen.contains(*s))
                    {
                        s.to_owned()
                    } else {
                        format!(
                            "{name}_{}_{}",
                            unix_millis(),
                            TOOL_COUNTER.fetch_add(1, Ordering::Relaxed) + 1
                        )
                    };
                    seen.insert(id.clone());
                    let arguments = function["args"]
                        .as_object()
                        .cloned()
                        .unwrap_or_else(Map::new);
                    let json = serde_json::to_string(&arguments)?;
                    let i = state.tool_start(ToolCall {
                        id,
                        name: name.into(),
                        arguments,
                        thought_signature: signature.map(str::to_owned),
                        ..ToolCall::default()
                    });
                    state.delta(i, json)?;
                    state.end(i);
                }
            }
        }
        let mut message = write_partial(&state.message);
        if let Some(raw) = candidate["finishReason"].as_str().filter(|s| !s.is_empty()) {
            message.raw_stop_reason = Some(raw.into());
            finish = Some(match raw {
                "STOP" => Ok(if seen.is_empty() {
                    StopReason::Stop
                } else {
                    StopReason::ToolUse
                }),
                "MAX_TOKENS" | "CONTINUATION" => Ok(StopReason::Length),
                "BLOCKLIST"
                | "PROHIBITED_CONTENT"
                | "SPII"
                | "SAFETY"
                | "IMAGE_SAFETY"
                | "IMAGE_PROHIBITED_CONTENT"
                | "IMAGE_RECITATION"
                | "IMAGE_OTHER"
                | "RECITATION"
                | "FINISH_REASON_UNSPECIFIED"
                | "OTHER"
                | "LANGUAGE"
                | "MALFORMED_FUNCTION_CALL"
                | "UNEXPECTED_TOOL_CALL"
                | "TOO_MANY_TOOL_CALLS"
                | "NO_IMAGE"
                | "MISSING_THOUGHT_SIGNATURE"
                | "MALFORMED_RESPONSE"
                | "ESCALATION"
                | "PUP_LIMITED_DISABLED"
                | "MODEL_ARMOR" => Err(Error::Protocol(format!("Provider stopped with: {raw}"))),
                _ => Err(Error::Protocol(format!("Unhandled stop reason: {raw}"))),
            });
        }
        if let Some(raw) = chunk.get("usageMetadata").filter(|u| u.is_object()) {
            let n = |key: &str| raw[key].as_f64().unwrap_or_default();
            let thoughts = n("thoughtsTokenCount");
            let cache = n("cachedContentTokenCount");
            message.usage.input = n("promptTokenCount") - cache;
            message.usage.output = n("candidatesTokenCount") + thoughts;
            message.usage.cache_read = cache;
            message.usage.reasoning = Some(thoughts);
            message.usage.total_tokens = n("totalTokenCount");
            message.usage.calculate_cost(&model.cost);
        }
    }
    if let Some((i, _)) = current {
        state.end(i);
    }
    if let Some(result) = finish {
        return result;
    }
    if let Some(err) = prompt_block_error {
        return Err(Error::Protocol(err));
    }
    let chunk_label = if received_chunk_count == 1 {
        "chunk"
    } else {
        "chunks"
    };
    Err(Error::Protocol(format!(
        "{provider_name} stream ended without a finish reason after receiving {received_chunk_count} {chunk_label}"
    )))
}

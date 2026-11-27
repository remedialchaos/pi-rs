use std::path::PathBuf;
use std::time::Duration;

use axum::http::{HeaderValue, StatusCode};
use futures_util::StreamExt;
use pi_rs_telemetry::AttributeValue;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::common::{Server, TestResult, context, model, plain, sse, stream_options};
use pi_rs_ai::{
    Api, AssistantContent, AssistantMessage, ClientIdentity, Message, Models, ModelsOptions,
    StopReason, StreamOptions, Tool, ToolCall,
    auth::{Credential, CredentialStore, FileCredentialStore},
    catalog::Catalog,
    compat,
    faux::{self, FauxOptions},
    normalize_context,
    openai_compat::{detect_compat, normalize_completion_id},
    openai_completions,
    providers::builtin_provider_specs,
    proxy::resolve_http_proxy_url_for_target,
    simple_options, streaming_json, transform, utils,
};

#[test]
fn compat_detects_proxy_families_and_prefers_explicit_overrides() -> TestResult {
    let mut m = model(
        Api::OpenAiCompletions,
        "custom",
        "deepseek-chat",
        "https://API.DeepSeek.com/v1",
    )?;
    let c = detect_compat(&m);
    assert!(!c.supports_store);
    assert!(c.assistant_reasoning_content);
    assert_eq!(c.thinking_format, "deepseek");
    assert_eq!(c.max_tokens_field, "max_tokens");
    m.compat = Some(serde_json::from_value(
        json!({"supportsStore":true,"supportsDeveloperRole":true,"maxTokensField":"max_completion_tokens","requiresAssistantAfterToolResult":true}),
    )?);
    let c = detect_compat(&m);
    assert!(c.supports_store && c.developer && c.assistant_after_tool);
    assert_eq!(c.max_tokens_field, "max_completion_tokens");
    let owned = normalize_completion_id("call-one|fc_weird!", &m)
        .ok_or("expected normalized combined tool id")?;
    assert_eq!(owned, "call-one_fc_weird_");
    Ok(())
}

#[test]
fn transform_drops_foreign_thinking_and_inserts_missing_result_before_user() -> TestResult {
    let old = model(
        Api::AnthropicMessages,
        "anthropic",
        "old",
        "http://localhost:0",
    )?;
    let new = model(
        Api::OpenAiCompletions,
        "openai",
        "new",
        "http://localhost:0",
    )?;
    let mut assistant = AssistantMessage::pending(&old);
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content = serde_json::from_value(json!([
        {"type":"thinking","thinking":"private","thinkingSignature":"opaque"},
        {"type":"text","text":"visible","textSignature":"foreign"},
        {"type":"toolCall","id":"id/foreign","name":"read","arguments":{"x":1},"thoughtSignature":"foreign"}
    ]))?;
    let mut messages = vec![Message::Assistant(assistant)];
    messages.extend(context()?.messages);
    let transformed =
        transform::transform_messages(messages, &new, |id, _, _| Some(id.replace('/', "_")));
    let (Message::Assistant(a), Message::ToolResult(result), Message::User(_)) =
        (&transformed[0], &transformed[1], &transformed[2])
    else {
        return Err("tool result omitted or moved".into());
    };
    assert_eq!(a.content.len(), 2);
    let AssistantContent::Text(text) = &a.content[0] else {
        return Err("visible text lost".into());
    };
    assert!(text.text_signature.is_none());
    let AssistantContent::ToolCall(call) = &a.content[1] else {
        return Err("expected surviving tool call".into());
    };
    assert_eq!(call.id, "id_foreign");
    assert!(call.thought_signature.is_none());
    assert_eq!(result.tool_call_id, call.id);
    assert!(result.is_error);
    Ok(())
}

#[test]
fn streaming_json_preserves_paths_escapes_and_repairable_raw_tabs() -> TestResult {
    let raw = r#"{"path":"C:\Home\foo","tab":"a	b","unicode":"🦀"}"#;
    let repaired = streaming_json::parse_json_with_repair(raw)?;
    assert_eq!(repaired["path"], r"C:\Homeoo");
    assert_eq!(repaired["tab"], "a\tb");
    assert_eq!(repaired["unicode"], "🦀");
    let mut tool = streaming_json::ToolCallArgumentsStream::default();
    tool.append("{\"edits\":[{\"old\":\"a");
    let partial = tool
        .append("bc\",\"new\":\"x")
        .ok_or("no parseable prefix")?;
    assert_eq!(partial["edits"][0]["old"], "abc");
    tool.append("\"}]}");
    let (args, error) = tool.finish();
    assert_eq!(args["edits"][0]["new"], "x");
    assert!(error.is_none());
    let (_, bad) = streaming_json::parse_tool_call_arguments(r#"{"ok": false BAD}"#);
    assert!(bad.is_some());
    Ok(())
}

#[test]
fn tool_validation_coerces_without_mutating_transcript_arguments() -> TestResult {
    let tool: Tool = serde_json::from_value(
        json!({"name":"port","description":"Choose port","parameters":{"type":"object","properties":{"port":{"type":"integer","minimum":1},"when":{"type":"string","format":"date"}},"required":["port","when"],"additionalProperties":false}}),
    )?;
    let mut call: ToolCall = serde_json::from_value(
        json!({"type":"toolCall","name":"port","id":"the-call","arguments":{"port":"443","when":"2026-11-20"}}),
    )?;
    let actual = utils::validate_tool_call(std::slice::from_ref(&tool), &call)?;
    assert_eq!(actual["port"].as_f64(), Some(443.0));
    assert_eq!(call.arguments["port"], "443");
    call.arguments.insert("when".into(), json!("2026-02-30"));
    let Err(invalid) = utils::validate_tool_arguments(&tool, &call) else {
        return Err("invalid date should fail".into());
    };
    let invalid = invalid.to_string();
    assert!(invalid.contains("  - when:") && invalid.contains("Received arguments:"));
    Ok(())
}

#[test]
fn context_pressure_clamps_output_but_excludes_rate_limits() -> TestResult {
    let mut m = model(Api::OpenAiCompletions, "test", "test", "http://localhost:0")?;
    m.context_window = 128;
    let large = "🦀".repeat(320);
    let ctx = serde_json::from_value(
        json!({"messages":[{"role":"user","timestamp":1,"content":large}]}),
    )?;
    assert_eq!(
        simple_options::clamp_max_tokens_to_context(&m, &normalize_context(ctx), 4096),
        1
    );
    assert_eq!(
        utils::sanitize_surrogates([0xD800, 0x0041, 0xDC00, 0xD83E, 0xDD80]),
        "A🦀"
    );
    let mut msg = AssistantMessage::pending(&m);
    msg.stop_reason = StopReason::Error;
    msg.error_message = Some("Throttling error: too many tokens".into());
    assert!(!utils::is_context_overflow(&msg, None));
    msg.error_message = Some("prompt is too long: 100001 tokens > 100000 maximum".into());
    assert!(utils::is_context_overflow(&msg, None));
    msg.stop_reason = StopReason::Length;
    msg.usage.input = 127.0;
    assert!(utils::is_context_overflow(&msg, Some(128)));
    assert!(utils::is_recoverable_length(&msg, 8));
    Ok(())
}

fn response_sse() -> axum::response::Response {
    sse(
        &[json!({"id":"id","choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]})],
        "\n",
    )
}
#[tokio::test]
async fn retry_honors_provider_delay_cap_and_abort_during_backoff() -> TestResult {
    let mut retry = plain(StatusCode::TOO_MANY_REQUESTS, "try again");
    retry
        .headers_mut()
        .insert("retry-after-ms", HeaderValue::from_static("30000"));
    let mut quick = plain(StatusCode::TOO_MANY_REQUESTS, "try again");
    quick
        .headers_mut()
        .insert("retry-after-ms", HeaderValue::from_static("2"));
    let mut wait = plain(StatusCode::TOO_MANY_REQUESTS, "try again");
    wait.headers_mut()
        .insert("retry-after-ms", HeaderValue::from_static("30000"));
    let server = Server::start(vec![retry, quick, response_sse(), wait]).await?;
    let m = model(Api::OpenAiCompletions, "test", "retry", &server.url)?;
    let mut opts = stream_options()?;
    opts.request.max_retries = Some(2);
    opts.request.max_retry_delay_ms = Some(20);
    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        openai_completions::stream(m.clone(), normalize_context(context()?), opts.clone()).result(),
    )
    .await??;
    assert_eq!(outcome.stop_reason, StopReason::Error);
    assert!(
        outcome
            .error_message
            .as_deref()
            .is_some_and(|s| s.contains("Server requested 30s retry delay (max: 1s)"))
    );
    assert_eq!(server.drain()?.len(), 1);
    let out = openai_completions::stream(m.clone(), normalize_context(context()?), opts.clone())
        .result()
        .await?;
    assert_eq!(out.stop_reason, StopReason::Stop);
    assert_eq!(server.drain()?.len(), 2);
    opts.request.max_retry_delay_ms = Some(40000);
    let abort = opts.request.signal.clone();
    let stream = openai_completions::stream(m, normalize_context(context()?), opts);
    tokio::time::sleep(Duration::from_millis(60)).await;
    abort.cancel();
    let aborted = tokio::time::timeout(Duration::from_secs(1), stream.result()).await??;
    assert_eq!(aborted.stop_reason, StopReason::Aborted);
    assert_eq!(server.drain()?.len(), 1);
    Ok(())
}
#[tokio::test]
async fn scoped_proxy_handles_no_proxy_and_forwards_http_absolute_uri() -> TestResult {
    let server = Server::start(vec![response_sse()]).await?;
    let mut opts = stream_options()?;
    opts.request.client = None;
    opts.request
        .env
        .insert("http_proxy".into(), server.url.clone());
    opts.request
        .env
        .insert("no_proxy".into(), ".bypass.invalid:8811".into());
    assert!(
        resolve_http_proxy_url_for_target("http://a.bypass.invalid:8811/path", &opts.request.env)?
            .is_none()
    );
    assert!(
        resolve_http_proxy_url_for_target("http://a.bypass.invalid:8812/path", &opts.request.env)?
            .is_some()
    );
    let m = model(
        Api::OpenAiCompletions,
        "custom",
        "proxied",
        "http://unresolvable.invalid/v1",
    )?;
    let out = openai_completions::stream(m, normalize_context(context()?), opts)
        .result()
        .await?;
    assert_eq!(out.stop_reason, StopReason::Stop, "{out:?}");
    let captures = server.drain()?;
    assert_eq!(
        captures[0].path,
        "http://unresolvable.invalid/v1/chat/completions"
    );
    Ok(())
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> TestResult<Self> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("fixture-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn hydrated_catalog_retains_chat_without_discriminator_and_reads_embedded_types() -> TestResult {
    let catalog = Catalog::embedded()?;
    assert_eq!(builtin_provider_specs()?.len(), 43);
    assert!(catalog.get_model("openai", "chat-latest").is_some());
    assert!(
        catalog
            .get_classifier_model("openai", "gpt-6-luna")
            .is_some()
    );
    assert_eq!(
        catalog
            .image_models("openrouter")
            .ok_or("missing OpenRouter images")?
            .len(),
        74
    );
    let scratch = Scratch::new()?;
    std::fs::write(scratch.0.join(".manifest.json"), r#"{"schemaVersion":6}"#)?;
    let m = model(
        Api::OpenAiResponses,
        "openai",
        "fixture",
        "http://localhost:0",
    )?;
    std::fs::write(
        scratch.0.join("openai.json"),
        serde_json::to_vec(&json!({"openai-responses":{"chat:fixture":m}}))?,
    )?;
    let hydrated = Catalog::from_hydrated_dir(&scratch.0)?;
    assert_eq!(hydrated.get_model("openai", "fixture"), Some(&m));
    Ok(())
}
#[tokio::test]
async fn file_credentials_mutate_atomically_and_keep_private_permissions() -> TestResult {
    let dir = Scratch::new()?;
    let path = dir.0.join("auth.json");
    let one = FileCredentialStore::new(path.clone());
    let two = FileCredentialStore::new(path.clone());
    let a: Credential = serde_json::from_value(
        json!({"type":"oauth","access":"alpha","refresh":"refresh","expires":20000}),
    )?;
    let b: Credential = serde_json::from_value(json!({"type":"api_key","key":"beta"}))?;
    let signal = CancellationToken::new();
    tokio::try_join!(
        one.modify(
            "oauth",
            Box::new(move |_| Box::pin(async move { Ok(Some(a)) })),
            &signal
        ),
        two.modify(
            "key",
            Box::new(move |_| Box::pin(async move { Ok(Some(b)) })),
            &signal
        )
    )?;
    let on_disk: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(on_disk["oauth"]["type"], "oauth");
    assert_eq!(on_disk["key"]["key"], "beta");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
    }
    Ok(())
}
#[tokio::test]
async fn faux_deferred_poll_is_cached_and_telemetry_excludes_pending_usage() -> TestResult {
    let faux = faux::faux_provider(FauxOptions {
        pending_fetches: 1,
        poll_after_ms: Some(7),
        ..Default::default()
    });
    let m = faux.get_model(None).ok_or("no faux model")?.clone();
    faux.set_responses([
        faux::faux_assistant_message(vec![faux::faux_text("🦀 read result")]).into(),
    ]);
    let models = Models::new(ModelsOptions::default());
    models.set_provider(faux.provider.clone());
    let sink = pi_rs_telemetry::Context::new();
    let mut opts = StreamOptions {
        telemetry: Some(sink.clone()),
        session_id: Some("test".into()),
        deferred: Some(json!(true)),
        ..Default::default()
    };
    let pending = models
        .complete_simple(m.clone(), context()?, opts.clone())
        .await?;
    assert_eq!(pending.stop_reason, StopReason::Deferred);
    let handle = pending.deferred.clone().ok_or("no faux handle")?;
    assert_eq!(handle.poll_after_ms, Some(7));
    opts.deferred = None;
    let first = models
        .fetch_deferred(m.clone(), handle.clone(), opts.clone())
        .await?;
    assert_eq!(first.stop_reason, StopReason::Deferred);
    let mut events = models.stream_deferred(m.clone(), handle.clone(), opts);
    let mut saw_delta = false;
    while let Some(event) = events.next().await {
        if matches!(event, pi_rs_ai::AssistantMessageEvent::TextDelta { .. }) {
            saw_delta = true;
        }
    }
    let complete = events.result().await?;
    assert!(saw_delta);
    assert_eq!(complete.stop_reason, StopReason::Stop);
    assert!(complete.usage.cache_write > 0.0);
    assert_eq!(faux.state.call_count(), 1);
    assert_eq!(faux.state.deferred_fetch_count(), 2);
    models
        .cancel_deferred(m, handle, StreamOptions::default())
        .await?;
    assert_eq!(faux.state.cancelled_deferred().len(), 1);
    let spans = sink.get_spans();
    assert_eq!(
        spans.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        vec!["pi.ai.request", "pi.ai.poll", "pi.ai.poll"]
    );
    let blocks = spans[2]
        .events
        .iter()
        .find(|e| e.name == "pi.ai.block")
        .ok_or("missing telemetry block")?;
    assert_eq!(
        blocks.attributes["chars"],
        AttributeValue::Number("🦀 read result".encode_utf16().count() as f64)
    );
    assert!(!blocks.attributes.contains_key("content"));
    assert_eq!(
        sink.get_metrics()
            .iter()
            .filter(|m| m.name == "pi.ai.tokens")
            .count(),
        8
    );
    Ok(())
}
#[test]
fn identity_null_attribution_overrides_defaults_and_legacy_lookup_needs_no_network() -> TestResult {
    let inherited: ClientIdentity = serde_json::from_value(
        json!({"attribution":{"name":"example","url":"https://example.test"},"version":"a"}),
    )?;
    let more: ClientIdentity =
        serde_json::from_value(json!({"attribution":null,"userAgent":{"custom":"test"}}))?;
    let merged = pi_rs_ai::registry::merge_identity(&inherited, more);
    assert_eq!(
        serde_json::to_value(&merged)?,
        json!({"userAgent":{"custom":"test"},"version":"a","attribution":null})
    );
    assert_eq!(
        compat::get_model("openai", "chat-latest")?.map(|m| m.api.clone()),
        Some(Api::OpenAiResponses)
    );
    assert_eq!(compat::get_api_providers().len(), 10);
    Ok(())
}
#[tokio::test]
async fn radius_catalog_refresh_replaces_shipped_models_and_recovers_offline() -> TestResult {
    use pi_rs_ai::models_store::InMemoryModelsStore;
    use pi_rs_ai::registry::RefreshOptions;
    use std::sync::Arc;
    let response = crate::common::json_reply(
        json!({"baseUrl":"http://localhost:0","models":[{"id":"gateway","name":"A Gateway", "input":["text"],"reasoning":false,"contextWindow":32768,"maxTokens":1000,
        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}}]}),
    );
    let server = Server::start(vec![response]).await?;
    let provider =
        pi_rs_ai::providers::radius_provider("fixture", "Fixture", Url::parse(&server.url)?)?;
    let store = Arc::new(InMemoryModelsStore::default());
    let opts = ModelsOptions {
        store,
        client: Some(reqwest::Client::builder().no_proxy().build()?),
        ..Default::default()
    };
    let models = Models::new(opts.clone());
    models.set_provider(provider.clone());
    let refresh = models.refresh(RefreshOptions::default()).await;
    assert!(refresh.errors.is_empty(), "{:?}", refresh.errors);
    assert_eq!(refresh.refreshed, ["fixture"]);
    let model = models
        .get_model("fixture", "gateway")
        .ok_or("model not published")?;
    assert_eq!(model.api, Api::PiMessages);
    let offline = Models::new(opts);
    offline.set_provider(provider);
    offline
        .refresh(RefreshOptions {
            allow_network: false,
            ..Default::default()
        })
        .await;
    assert_eq!(offline.get_model("fixture", "gateway"), Some(model));
    assert_eq!(server.drain()?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn openrouter_personalization_limits_only_chat_and_uses_prompt_tiers() -> TestResult {
    use pi_rs_ai::{
        AnyModel, providers::personalize_openrouter_catalog, registry::ModelSourceRequest,
    };
    let payload = json!({"data":[{"id":"allowed","context_length":50000,
    "top_provider":{"max_completion_tokens":1000},
    "pricing":{"prompt":"0.000001","completion":"0.000002","overrides":[
        {"min_prompt_tokens":20000,"prompt":"0.000003"},
        {"min_prompt_tokens":30000,"prompt":"0.000008","utc_start":3}
    ]}}]});
    let server = Server::start(vec![crate::common::json_reply(payload)]).await?;
    let allowed = model(Api::OpenAiCompletions, "openrouter", "allowed", &server.url)?;
    let mut other = allowed.clone();
    other.id = "blocked".into();
    let image = Catalog::embedded()?
        .image_models("openrouter")
        .and_then(|m| m.values().next())
        .ok_or("no image catalog")?
        .clone();
    let credential: Credential =
        serde_json::from_value(json!({"type":"api_key","key":"fixture-key"}))?;
    let client = reqwest::Client::builder().no_proxy().build()?;
    let signal = CancellationToken::new();
    let result = personalize_openrouter_catalog(
        vec![
            AnyModel::Chat(allowed),
            AnyModel::Chat(other),
            AnyModel::Image(image.clone()),
        ],
        ModelSourceRequest {
            provider: "openrouter",
            base_url: Some(&server.url),
            credential: Some(&credential),
            stored: None,
            allow_network: true,
            client: &client,
            signal: &signal,
        },
    )
    .await?;
    assert_eq!(result.len(), 2);
    let AnyModel::Chat(chat) = &result[0] else {
        return Err("chat omitted".into());
    };
    assert_eq!((chat.context_window, chat.max_tokens), (50000, 1000));
    assert_eq!(chat.cost.tiers.len(), 1);
    assert_eq!(
        (
            chat.cost.tiers[0].rates.input,
            chat.cost.tiers[0].rates.output
        ),
        (3.0, 2.0)
    );
    assert_eq!(result[1], AnyModel::Image(image));
    assert_eq!(server.drain()?[0].path, "/models/user");
    Ok(())
}

#[tokio::test]
async fn compat_dispatches_built_in_providers_through_the_model_registry() -> TestResult {
    // A built-in provider resolves its session header through `Models`, like upstream's
    // `createModels()` path: the direct API path never sent it.
    let server = Server::start(vec![sse(
        &[
            json!({"type":"message_start","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":1,"output_tokens":1}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
            json!({"type":"message_stop"}),
        ],
        "\r\n",
    )])
    .await?;
    let mut options = stream_options()?;
    options.request.api_key = Some("fixture-key".into());
    options.request.client = Some(reqwest::Client::builder().no_proxy().build()?);
    options.session_id = Some("session-1".into());
    let model = model(
        Api::AnthropicMessages,
        "opencode",
        "claude-fable-5",
        &server.url,
    )?;
    let stream = compat::stream(model, context()?, options)?;
    let result = stream.result().await?;
    assert_eq!(result.stop_reason, StopReason::Stop);
    let request = &server.drain()?[0];
    assert_eq!(request.headers["x-opencode-session"], "session-1");
    assert_eq!(request.headers["x-api-key"], "fixture-key");
    Ok(())
}

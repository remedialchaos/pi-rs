use std::sync::{Arc, Mutex};

use axum::{Json, Router, extract::State, response::IntoResponse, routing::post};
use indexmap::IndexMap;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::common::{Server, TestResult, json_reply, request};
use pi_rs_ai::{
    ClassifierContext, ClassifierModel, ImageModel, ImagesContext, InputContent, Models,
    ModelsOptions, StopReason,
    auth::CredentialStore,
    classifier::{self, ClassifierOptions},
    llama_cpp, openai_decisions, openrouter_images,
};

fn questions() -> TestResult<ClassifierContext> {
    Ok(serde_json::from_value(
        json!({"state":{"message":"hello"},"questions":{"ok":{"type":"bool","instructions":"Is this good?","criteria":{"true":"Approve","false":"Reject"}}}}),
    )?)
}
fn classifier_model(api: &str, provider: &str, base: &str) -> TestResult<ClassifierModel> {
    Ok(serde_json::from_value(
        json!({"type":"classifier","api":api,"provider":provider,"baseUrl":base,"id":"fixture","name":"Fixture","input":["text","image"],"contextWindow":32768,"cost":{"input":1,"output":2,"cacheRead":0.1,"cacheWrite":1.25}}),
    )?)
}
#[tokio::test]
async fn typesafe_uses_named_assets_and_prices_reported_usage() -> TestResult {
    let server=Server::start(vec![json_reply(json!({"answers":{"ok":{"type":"noul","noul":0.7}},"usage":{"input_tokens":12,"output_tokens":2}}))]).await?;
    let model = classifier_model(
        "typesafe-system-one",
        "typesafe",
        &format!("{}/v1", server.url),
    )?;
    let mut context = questions()?;
    context.images.push(serde_json::from_value(
        json!({"type":"image","mimeType":"image/png","data":"YQ=="}),
    )?);
    let result = classifier::classify_system_one(
        &model,
        &context,
        &ClassifierOptions {
            request: request()?,
            temperature: Some(0.2),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{result:?}");
    assert_eq!(
        serde_json::to_value(&result.answers["ok"])?["probability"],
        0.7
    );
    assert!((result.usage.ok_or("no reported usage")?.cost.total - 0.000016).abs() < 1e-12);
    let calls = server.drain()?;
    let req = &calls[0];
    assert_eq!(req.path, "/v1/systemone");
    assert_eq!(req.headers["typesafe-danger-zone-images"], "true");
    let body = req.json()?;
    assert_eq!(body["assets"]["<image-1>"]["data_base64"], "YQ==");
    assert_eq!(body["questions"]["ok"]["type"], "noul");
    assert!(body.get("temperature").is_none());
    Ok(())
}
#[tokio::test]
async fn cloudflare_workers_materializes_account_and_unwraps_run_record() -> TestResult {
    let result = json!({"answers":{"ok":{"type":"noul","noul":0.4}},"usage":{"input_tokens":17}});
    let server = Server::start(vec![json_reply(
        json!({"success":true,"result":{"state":"Completed","result":result}}),
    )])
    .await?;
    let model = classifier_model(
        "cloudflare-workers-ai-system-one",
        "cloudflare-workers-ai",
        &format!("{}/accounts/{{CLOUDFLARE_ACCOUNT_ID}}/ai", server.url),
    )?;
    let models = Models::builtin(ModelsOptions::default())?;
    let mut opts = ClassifierOptions {
        request: request()?,
        ..Default::default()
    };
    opts.request
        .env
        .insert("CLOUDFLARE_ACCOUNT_ID".into(), "account-fixture".into());
    let result = models.classify(model, &questions()?, opts).await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{result:?}");
    assert_eq!(
        serde_json::to_value(&result.answers["ok"])?["probability"],
        0.4
    );
    let req = server.drain()?;
    assert_eq!(req[0].path, "/accounts/account-fixture/ai/run");
    assert_eq!(req[0].json()?["input"]["questions"]["ok"]["type"], "noul");
    Ok(())
}
#[tokio::test]
async fn openai_decisions_splits_cache_and_maps_predicate_by_name() -> TestResult {
    let server=Server::start(vec![json_reply(json!({"answers":[{"type":"predicate","name":"ok","probability":0.8}],"usage":{"input_tokens":12,"output_tokens":2,"input_tokens_details":{"cached_tokens":3,"cache_write_tokens":2}}}))]).await?;
    let m = classifier_model("openai-decisions", "openai", &format!("{}/v1", server.url))?;
    let output = openai_decisions::classify(
        &m,
        &questions()?,
        &ClassifierOptions {
            request: request()?,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(output.stop_reason, StopReason::Stop, "{output:?}");
    assert_eq!(
        serde_json::to_value(&output.answers["ok"])?["probability"],
        0.8
    );
    let usage = output.usage.ok_or("missing decisions usage")?;
    assert_eq!(
        (usage.input, usage.cache_read, usage.cache_write),
        (7.0, 3.0, 2.0)
    );
    let req = server.drain()?;
    assert_eq!(req[0].path, "/v1/decisions");
    let body = req[0].json()?;
    assert_eq!(body["questions"][0]["type"], "predicate");
    assert!(
        body["questions"][0]["instructions"]
            .as_str()
            .is_some_and(|s| s.contains("True means: Approve"))
    );
    Ok(())
}
#[tokio::test]
async fn llama_classifier_requests_deeper_probabilities_for_missing_labels() -> TestResult {
    let depths = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route(
            "/tokenize",
            post(|Json(body): Json<Value>| async move {
                Json(match body["content"].as_str() {
                    Some("\nYes") => json!({"tokens":[9,10]}),
                    Some("\nNo") => json!({"tokens":[9,11]}),
                    _ => json!({"tokens":[9]}),
                })
            }),
        )
        .route(
            "/apply-template",
            post(|| async { Json(json!({"prompt":"fixture<think>"})) }),
        )
        .route(
            "/completion",
            post(
                |State(depths): State<Arc<Mutex<Vec<u64>>>>, Json(body): Json<Value>| async move {
                    let depth = body["n_probs"].as_u64().unwrap_or_default();
                    if let Ok(mut captured) = depths.lock() {
                        captured.push(depth);
                    }
                    let output = if body["prompt"] != "fixture<think></think>" {
                        json!([])
                    } else if depth == 256 {
                        json!([{"id":10,"logprob":-0.5}])
                    } else {
                        json!([{"id":10,"logprob":-0.5},{"id":11,"logprob":-1.5}])
                    };
                    Json(json!({"completion_probabilities":[{"top_logprobs":output}]}))
                },
            ),
        )
        .with_state(depths.clone());
    let socket = TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/v1", socket.local_addr()?);
    let server = tokio::spawn(async move {
        let _ = axum::serve(socket, app).await;
    });
    let m = classifier_model("llama-cpp-classify", "llama-cpp", &base)?;
    let result = llama_cpp::classify(
        &m,
        &questions()?,
        &ClassifierOptions {
            request: request()?,
            temperature: Some(1.0),
            ..Default::default()
        },
    )
    .await;
    server.abort();
    assert_eq!(result.stop_reason, StopReason::Stop, "{result:?}");
    let v = serde_json::to_value(&result.answers["ok"])?;
    assert!(
        (v["probability"].as_f64().ok_or("missing probability")? - 0.7310585786300049).abs()
            < 1e-12
    );
    assert_eq!(
        *depths.lock().map_err(|_| "poisoned test log")?,
        vec![256, 4096]
    );
    Ok(())
}
#[tokio::test]
async fn openrouter_image_generation_decodes_data_urls_and_uses_catalog_cost() -> TestResult {
    let server=Server::start(vec![json_reply(json!({"id":"image-1","choices":[{"finish_reason":"stop","message":{"content":"caption","images":[{"type":"image_url","image_url":{"url":"data:image/png;base64,YQ=="}}]}}],
        "usage":{"prompt_tokens":12,"completion_tokens":5,"total_tokens":17,"cost":0.42}}))]).await?;
    let model: ImageModel = serde_json::from_value(
        json!({"type":"image","api":"openrouter-images","id":"fixture","provider":"openrouter","name":"Fixture","baseUrl":format!("{}/api/v1",server.url),"input":["text","image"],"output":["image","text"],"cost":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0}}),
    )?;
    let context: ImagesContext = serde_json::from_value(
        json!({"input":[{"type":"text","text":"draw a dot"},{"type":"image","mimeType":"image/png","data":"Yg=="}]}),
    )?;
    let output = openrouter_images::generate_images(&model, &context, &request()?).await;
    assert_eq!(output.stop_reason, StopReason::Stop, "{output:?}");
    assert_eq!(output.response_id.as_deref(), Some("image-1"));
    assert!(
        output.output.iter().any(
            |b| matches!(b,InputContent::Image(i) if i.data=="YQ=="&&i.mime_type=="image/png")
        )
    );
    let cost = output.usage.ok_or("missing image usage")?.cost;
    assert!((cost.total - 0.000022).abs() < 1e-12);
    assert_eq!(cost.total_from_provider, Some(false));
    let captures = server.drain()?;
    assert_eq!(captures[0].path, "/api/v1/chat/completions");
    let wire = captures[0].json()?;
    assert_eq!(wire["modalities"], json!(["image", "text"]));
    assert_eq!(
        wire["messages"][0]["content"][1]["image_url"]["url"],
        "data:image/png;base64,Yg=="
    );
    Ok(())
}

#[tokio::test]
async fn pi_decision_posts_to_decision_validates_answers_and_handles_images() -> TestResult {
    let mut ctx = questions()?;
    ctx.questions.insert(
        "sentiment".into(),
        pi_rs_ai::ClassifierQuestion::Choice {
            instructions: "Sentiment?".into(),
            criteria: IndexMap::from([
                ("positive".into(), "Positive".into()),
                ("negative".into(), "Negative".into()),
            ]),
        },
    );
    ctx.questions.insert(
        "urgency".into(),
        pi_rs_ai::ClassifierQuestion::Score {
            instructions: "Urgency?".into(),
            criteria: vec!["low".into(), "medium".into(), "high".into()],
        },
    );
    let server = Server::start(vec![
        json_reply(json!({
            "answers": {
                "ok": { "type": "bool", "probability": 0.96 },
                "sentiment": {
                    "type": "choice",
                    "choice": "positive",
                    "probabilities": { "positive": 0.95, "negative": 0.05 },
                    "confidence": 0.9
                },
                "urgency": { "type": "score", "score": 0.4, "confidence": 0.8 }
            },
            "usage": {
                "input": 120,
                "output": 3,
                "cacheRead": 0,
                "cacheWrite": 0,
                "totalTokens": 123,
                "cost": { "input": 0.0001, "output": 0.00001, "cacheRead": 0, "cacheWrite": 0, "total": 0.00011 }
            },
            "responseId": "gen-dec-1"
        })),
        (
            axum::http::StatusCode::PAYMENT_REQUIRED,
            Json(json!({ "error": { "message": "Insufficient credits", "code": "insufficient_credits" } })),
        )
            .into_response(),
        json_reply(json!({
            "answers": {
                "ok": { "type": "bool", "probability": 1.5 },
                "sentiment": {
                    "type": "choice",
                    "choice": "positive",
                    "probabilities": { "positive": 0.9, "negative": 0.1 },
                    "confidence": 0.8
                },
                "urgency": { "type": "score", "score": 1, "confidence": 0.5 }
            }
        })),
    ])
    .await?;
    let m = classifier_model("pi-decision", "radius", &format!("{}/v1", server.url))?;
    let mut opts = ClassifierOptions {
        request: request()?,
        session_id: Some("sess-1".into()),
        ..Default::default()
    };
    let ok = pi_rs_ai::pi_decision::classify(&m, &ctx, &opts).await;
    assert_eq!(ok.stop_reason, StopReason::Stop, "{ok:?}");
    assert_eq!(ok.response_id.as_deref(), Some("gen-dec-1"));
    assert!((ok.usage.ok_or("missing usage")?.cost.total - 0.00011).abs() < 1e-12);
    let calls = server.drain()?;
    assert_eq!(calls[0].path, "/v1/decision");
    assert_eq!(calls[0].json()?["options"]["sessionId"], "sess-1");

    opts.request.max_retries = Some(0);
    let err_env = pi_rs_ai::pi_decision::classify(&m, &ctx, &opts).await;
    assert_eq!(err_env.stop_reason, StopReason::Error);
    assert!(
        err_env
            .error_message
            .as_deref()
            .is_some_and(|s| s.contains("Insufficient credits"))
    );

    let bad_prob = pi_rs_ai::pi_decision::classify(&m, &ctx, &opts).await;
    assert_eq!(bad_prob.stop_reason, StopReason::Error);
    assert!(bad_prob.answers.is_empty());
    assert!(
        bad_prob
            .error_message
            .as_deref()
            .is_some_and(|s| s.contains("out-of-range probability"))
    );

    let mut img_ctx = ctx.clone();
    img_ctx.images.push(pi_rs_ai::ImageContent {
        kind: Default::default(),
        data: "AAAA".into(),
        mime_type: "image/png".into(),
    });
    let mut text_only = m.clone();
    text_only.input = vec![pi_rs_ai::ModelInputModality::Text];
    let rejected = pi_rs_ai::pi_decision::classify(&text_only, &img_ctx, &opts).await;
    assert_eq!(rejected.stop_reason, StopReason::Error);
    assert!(
        rejected
            .error_message
            .as_deref()
            .is_some_and(|s| s.contains("does not support image input"))
    );
    Ok(())
}

#[tokio::test]
async fn openrouter_system_one_exposes_response_id_and_billed_cost() -> TestResult {
    let server = Server::start(vec![json_reply(json!({
        "id": "gen-dec-or-1",
        "answers": { "ok": { "type": "noul", "noul": 0.95 } },
        "usage": {
            "input_tokens": 304,
            "output_tokens": 20,
            "cost": 0.000012768,
            "is_byok": false
        }
    }))])
    .await?;
    let m = classifier_model("typesafe-system-one", "openrouter", &server.url)?;
    let out = classifier::classify_system_one(
        &m,
        &questions()?,
        &ClassifierOptions {
            request: request()?,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(out.stop_reason, StopReason::Stop, "{out:?}");
    assert_eq!(out.response_id.as_deref(), Some("gen-dec-or-1"));
    let usage = out.usage.ok_or("missing usage")?;
    assert!((usage.cost.total - 0.000012768).abs() < 1e-12);
    assert_eq!(usage.cost.total_from_provider, Some(true));
    Ok(())
}

#[tokio::test]
async fn copilot_gpt6_luna_decisions_routes_v1_and_filters_by_account() -> TestResult {
    let creds = Arc::new(pi_rs_ai::auth::InMemoryCredentialStore::default());
    let mut details = serde_json::Map::new();
    details.insert("availableModelIds".into(), json!(["gpt-6-luna"]));
    details.insert(
        "availableClassifierModelIds".into(),
        json!(["gpt-6-luna-decisions"]),
    );
    creds
        .modify(
            "github-copilot",
            Box::new(move |_| {
                Box::pin(async move {
                    Ok(Some(pi_rs_ai::auth::Credential::OAuth(
                        pi_rs_ai::auth::OAuthCredential {
                            access:
                                "tid=test;exp=9999999999;proxy-ep=proxy.business.githubcopilot.com;"
                                    .into(),
                            refresh: "refresh".into(),
                            expires: u64::MAX,
                            details,
                        },
                    )))
                })
            }),
            &CancellationToken::new(),
        )
        .await?;
    let models = Models::builtin(ModelsOptions {
        credentials: creds,
        ..Default::default()
    })?;
    let available = models
        .get_available_of_type(
            pi_rs_ai::ModelType::Classifier,
            Some("github-copilot"),
            &CancellationToken::new(),
        )
        .await?;
    assert_eq!(available.len(), 1);
    assert_eq!(available[0].id(), "gpt-6-luna-decisions");
    Ok(())
}

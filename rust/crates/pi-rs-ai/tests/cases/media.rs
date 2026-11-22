use std::sync::{Arc, Mutex};

use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::common::{Server, TestResult, json_reply, request};
use pi_rs_ai::{
    ClassifierContext, ClassifierModel, ImageModel, ImagesContext, InputContent, Models,
    ModelsOptions, StopReason,
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
        temperature: None,
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
            temperature: None,
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

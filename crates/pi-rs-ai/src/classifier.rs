use indexmap::IndexMap;
use reqwest::{Method, StatusCode};
use serde_json::{Map, Value, json};
use url::Url;

use crate::error::{Error, Result};
use crate::http::{HttpRequest, abortable, endpoint, payload, request_headers, send};
use crate::model::{ClassifierApi, ClassifierModel, ModelInputModality};
use crate::openrouter_images::image_data_url;
use crate::request::{ProviderHeaders, ProviderRequestOptions};
use crate::types::{
    ClassifierAnswer, ClassifierContext, ClassifierQuestion, ClassifierResult, StopReason, Usage,
    unix_millis,
};

#[derive(Clone, Default)]
pub struct ClassifierOptions {
    pub request: ProviderRequestOptions<ClassifierModel>,
    pub temperature: Option<f64>,
}

pub async fn classify_system_one(
    model: &ClassifierModel,
    context: &ClassifierContext,
    options: &ClassifierOptions,
) -> ClassifierResult {
    let mut output = classifier_output(model);
    if let Err(e) = run_system_one(&mut output, model, context, &options.request).await {
        classifier_failed(&mut output, e, &options.request);
    }
    output
}

pub(crate) fn classifier_output(model: &ClassifierModel) -> ClassifierResult {
    ClassifierResult {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        answers: IndexMap::new(),
        usage: None,
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: unix_millis(),
    }
}

pub(crate) fn classifier_failed(
    output: &mut ClassifierResult,
    error: Error,
    options: &ProviderRequestOptions<ClassifierModel>,
) {
    output.stop_reason = if options.signal.is_cancelled() || matches!(error, Error::Aborted) {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    output.error_message = Some(error.to_string());
}

pub(crate) async fn post_classifier_request(
    model: &ClassifierModel,
    options: &ProviderRequestOptions<ClassifierModel>,
    url: Url,
    body: Value,
    label: &str,
    default_headers: ProviderHeaders,
    no_retry_statuses: &[StatusCode],
) -> Result<Value> {
    let key = options
        .api_key
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Auth(format!("No API key for provider: {}", model.provider)))?;
    let body = payload(body, model, options).await?;
    let mut default_headers = default_headers;
    default_headers.insert("authorization".into(), Some(format!("Bearer {key}")));
    default_headers.insert("content-type".into(), Some("application/json".into()));
    let request = HttpRequest {
        method: Method::POST,
        url,
        headers: request_headers(&model.base, options, default_headers)?,
        body: Some(serde_json::to_vec(&body)?),
        error_label: format!("{label} error"),
        force_http1: false,
        default_retries: 2,
        no_retry_statuses: no_retry_statuses.to_vec(),
    };
    let response = send(request, model, options).await?;
    abortable(&options.signal, async {
        Ok(response.json::<Value>().await?)
    })
    .await
}

pub(crate) fn parse_classifier_usage(raw: &Value, model: &ClassifierModel) -> Option<Usage> {
    if raw.get("input_tokens").is_none() && raw.get("output_tokens").is_none() {
        return None;
    }
    let token = |v: &Value| {
        v.as_f64()
            .filter(|n| n.is_finite() && *n > 0.0)
            .unwrap_or(0.0)
    };
    let total_input = token(&raw["input_tokens"]);
    let output = token(&raw["output_tokens"]);
    let details = &raw["input_tokens_details"];
    let cache_read = token(&details["cached_tokens"]).min(total_input);
    let cache_write = token(&details["cache_write_tokens"]).min(total_input - cache_read);
    let mut usage = Usage {
        input: total_input - cache_read - cache_write,
        output,
        cache_read,
        cache_write,
        total_tokens: total_input + output,
        ..Usage::default()
    };
    usage.calculate_cost(&model.cost);
    Some(usage)
}

fn wire_question(question: &ClassifierQuestion) -> Result<Value> {
    let mut wire = serde_json::to_value(question)?;
    if matches!(question, ClassifierQuestion::Bool { .. }) {
        wire["type"] = json!("noul");
    }
    Ok(wire)
}
async fn run_system_one(
    output: &mut ClassifierResult,
    model: &ClassifierModel,
    context: &ClassifierContext,
    options: &ProviderRequestOptions<ClassifierModel>,
) -> Result<()> {
    let cloudflare = match model.api {
        ClassifierApi::TypeSafeSystemOne => false,
        ClassifierApi::CloudflareWorkersAiSystemOne => true,
        _ => {
            return Err(Error::Protocol(format!(
                "Unsupported classifier API: {}",
                model.api
            )));
        }
    };
    let label = if cloudflare {
        "Cloudflare Workers AI"
    } else {
        "System One API"
    };
    let questions = context
        .questions
        .iter()
        .map(|(key, question)| wire_question(question).map(|q| (key.clone(), q)))
        .collect::<Result<Map<_, _>>>()?;
    let mut body = json!({"state":context.state, "questions":questions});
    let mut headers = ProviderHeaders::new();
    if !context.images.is_empty() {
        if !model.input.contains(&ModelInputModality::Image) {
            return Err(Error::Protocol(format!(
                "{label} does not support image input for {}",
                model.id
            )));
        }
        let openrouter = !cloudflare
            && (model.provider == "openrouter" || model.base_url.contains("openrouter.ai"));
        let detected = if openrouter {
            "state-image-urls"
        } else if cloudflare && model.id.starts_with("@cf/cloudflare/clef") {
            "images"
        } else {
            "assets"
        };
        let field = |key| model.compat.as_ref().and_then(|object| object.get(key));
        let input = field("imageInput")
            .and_then(Value::as_str)
            .unwrap_or(detected);
        if field("sendImageOptInHeader")
            .and_then(Value::as_bool)
            .unwrap_or(!openrouter && !cloudflare)
        {
            headers.insert("TypeSafe-Danger-Zone-Images".into(), Some("true".into()));
        }
        match input {
            "images" => {
                if context.images.len() > 4 {
                    return Err(Error::Protocol(format!(
                        "{label} accepts at most 4 images for {}, got {}",
                        model.id,
                        context.images.len()
                    )));
                }
                let mut urls = Vec::with_capacity(context.images.len());
                for image in &context.images {
                    if !matches!(
                        image.mime_type.as_str(),
                        "image/png" | "image/jpeg" | "image/webp"
                    ) {
                        return Err(Error::Protocol(format!(
                            "{label} accepts PNG, JPEG, and WebP images for {}, got {}",
                            model.id, image.mime_type
                        )));
                    }
                    urls.push(image_data_url(image));
                }
                body["images"] = json!(urls);
            }
            "state-image-urls" => {
                let mut parts = context.images.iter().map(|image| json!({"type":"image_url","image_url":{"url":image_data_url(image)}})).collect::<Vec<_>>();
                parts.push(body["state"].take());
                body["state"] = Value::Array(parts);
            }
            _ => {
                let mut assets = Map::with_capacity(context.images.len());
                let mut parts = Vec::with_capacity(context.images.len() + 1);
                for (i, image) in context.images.iter().enumerate() {
                    let format = match image.mime_type.as_str() {
                        "image/png" => "png",
                        "image/jpeg" => "jpeg",
                        _ => {
                            return Err(Error::Protocol(format!(
                                "{label} accepts PNG and JPEG images for {}, got {}",
                                model.id, image.mime_type
                            )));
                        }
                    };
                    let name = format!("<image-{}>", i + 1);
                    parts.push(json!(name));
                    assets.insert(
                        name,
                        json!({"type":"image","format":format,"data_base64":image.data}),
                    );
                }
                body["assets"] = Value::Object(assets);
                parts.push(body["state"].take());
                body["state"] = Value::Array(parts);
            }
        }
    }
    let url = endpoint(
        &model.base_url,
        if cloudflare { "run" } else { "systemone" },
    )?;
    let body = if cloudflare {
        json!({"model":model.id,"input":body})
    } else {
        let mut request = Map::new();
        request.insert("model".into(), json!(model.id));
        if let Value::Object(body) = body {
            request.extend(body);
        }
        Value::Object(request)
    };
    let response = post_classifier_request(model, options, url, body, label, headers, &[]).await?;
    let result = if cloudflare {
        cloudflare_output(&response)?
    } else {
        &response
    };
    output.usage = parse_classifier_usage(&result["usage"], model);
    output.answers = parse_answers(&result["answers"], &context.questions, label)?;
    Ok(())
}

fn cloudflare_output(body: &Value) -> Result<&Value> {
    const LABEL: &str = "Cloudflare Workers AI";
    let unexpected = || Error::Protocol(format!("{LABEL} returned an unexpected response"));
    if body["success"] == Value::Bool(false) {
        let messages = body["errors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["message"].as_str())
            .collect::<Vec<_>>();
        return Err(Error::Protocol(if messages.is_empty() {
            format!("{LABEL} request failed")
        } else {
            format!("{LABEL} error: {}", messages.join("; "))
        }));
    }
    let result = body
        .get("result")
        .filter(|r| r.is_object())
        .ok_or_else(unexpected)?;
    if result.get("answers").is_some() {
        return Ok(result);
    }
    let state = result["state"].as_str().unwrap_or("undefined");
    if state != "Completed" {
        return Err(Error::Protocol(format!(
            "{LABEL} run did not complete (state: {state})"
        )));
    }
    result
        .get("result")
        .filter(|r| r.is_object())
        .ok_or_else(unexpected)
}
fn parse_answers(
    value: &Value,
    questions: &IndexMap<String, ClassifierQuestion>,
    label: &str,
) -> Result<IndexMap<String, ClassifierAnswer>> {
    if !value.is_object() {
        return Err(Error::Protocol(format!(
            "{label} returned an unexpected response"
        )));
    }
    questions
        .iter()
        .map(|(id, q)| {
            let answer = value.get(id).filter(|a| a.is_object()).ok_or_else(|| {
                Error::Protocol(format!("{label} did not return an answer for {id}"))
            })?;
            let field = |name: &str, shown: &str| {
                answer[name].as_f64().ok_or_else(|| {
                    Error::Protocol(format!("{label} returned an invalid {shown} for {id}"))
                })
            };
            let kind = answer["type"].as_str();
            let wrong = |expected| {
                Error::Protocol(format!(
                    "{label} did not return a {expected} answer for {id}"
                ))
            };
            let parsed = match q {
                ClassifierQuestion::Choice { .. } => {
                    let choice = answer["choice"]
                        .as_str()
                        .filter(|_| kind == Some("choice"))
                        .ok_or_else(|| wrong("choice"))?;
                    let probabilities = answer["probabilities"].as_object().ok_or_else(|| {
                        Error::Protocol(format!("{label} returned invalid probabilities for {id}"))
                    })?;
                    let probabilities = probabilities
                        .iter()
                        .map(|(k, v)| {
                            v.as_f64().map(|n| (k.clone(), n)).ok_or_else(|| {
                                Error::Protocol(format!(
                                    "{label} returned an invalid probability for {id}.{k}"
                                ))
                            })
                        })
                        .collect::<Result<_>>()?;
                    ClassifierAnswer::Choice {
                        choice: choice.to_owned(),
                        probabilities,
                        confidence: field("confidence", "confidence")?,
                    }
                }
                ClassifierQuestion::Score { .. } if kind == Some("score") => {
                    ClassifierAnswer::Score {
                        score: field("score", "score")?,
                        confidence: field("confidence", "confidence")?,
                    }
                }
                ClassifierQuestion::Bool { .. } if kind == Some("noul") => ClassifierAnswer::Bool {
                    probability: field("noul", "probability")?,
                },
                ClassifierQuestion::Score { .. } => return Err(wrong("score")),
                ClassifierQuestion::Bool { .. } => return Err(wrong("bool")),
            };
            Ok((id.clone(), parsed))
        })
        .collect()
}

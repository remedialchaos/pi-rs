use reqwest::Method;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::http::{HttpRequest, abortable, endpoint, payload, request_headers, send};
use crate::model::{ImageModel, ModelInputModality};
use crate::request::{ProviderHeaders, ProviderRequestOptions};
use crate::types::{
    AssistantImages, Cost, ImageContent, ImagesContext, InputContent, StopReason, Usage,
    unix_millis,
};

pub type ImagesOptions = ProviderRequestOptions<ImageModel>;

pub async fn generate_images(
    model: &ImageModel,
    context: &ImagesContext,
    options: &ImagesOptions,
) -> AssistantImages {
    let mut output = AssistantImages {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        output: Vec::new(),
        response_id: None,
        usage: None,
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: unix_millis(),
    };
    if let Err(error) = generate(&mut output, model, context, options).await {
        output.stop_reason = if options.signal.is_cancelled() || matches!(error, Error::Aborted) {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        output.error_message = Some(error.to_string());
    }
    output
}

async fn generate(
    output: &mut AssistantImages,
    model: &ImageModel,
    ctx: &ImagesContext,
    options: &ImagesOptions,
) -> Result<()> {
    let api_key = options
        .api_key
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Auth(format!("No API key for provider: {}", model.provider)))?;
    let content = ctx
        .input
        .iter()
        .map(|item| match item {
            InputContent::Text(text) => json!({"type":"text","text":text.text}),
            InputContent::Image(image) => {
                json!({"type":"image_url", "image_url": {"url": image_data_url(image)}})
            }
        })
        .collect::<Vec<_>>();
    let modalities: &[&str] = if model.output.contains(&ModelInputModality::Text) {
        &["image", "text"]
    } else {
        &["image"]
    };
    let body = payload(json!({"model":model.id, "messages":[{"role":"user", "content":content}], "stream":false,"modalities":modalities}), model, options).await?;
    let defaults = ProviderHeaders::from([
        ("Authorization".into(), Some(format!("Bearer {api_key}"))),
        ("Content-Type".into(), Some("application/json".into())),
    ]);
    let request = HttpRequest {
        method: Method::POST,
        url: endpoint(&model.base_url, "chat/completions")?,
        headers: request_headers(&model.base, options, defaults)?,
        body: Some(serde_json::to_vec(&body)?),
        error_label: "OpenRouter Images API error".into(),
        force_http1: false,
        default_retries: 0,
        no_retry_statuses: Vec::new(),
    };
    let response = send(request, model, options).await?;
    let value = abortable(&options.signal, async {
        Ok(response.json::<Value>().await?)
    })
    .await?;
    output.response_id = value["id"].as_str().map(str::to_owned);
    if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        output.usage = Some(image_usage(usage, model));
    }
    let Some(choices) = value["choices"].as_array() else {
        return Err(Error::Protocol(
            "OpenRouter Images API returned an unexpected response".into(),
        ));
    };
    if let Some(choice) = choices.first() {
        let message = &choice["message"];
        if let Some(text) = message["content"].as_str().filter(|s| !s.is_empty()) {
            output.output.push(InputContent::Text(text.into()));
        }
        for image in message["images"].as_array().into_iter().flatten() {
            let url = image["image_url"]
                .as_str()
                .or_else(|| image["image_url"]["url"].as_str());
            let Some((mime, data)) = url
                .and_then(|u| u.strip_prefix("data:"))
                .and_then(|u| u.split_once(";base64,"))
            else {
                continue;
            };
            if !mime.is_empty()
                && !mime.contains(';')
                && !data.is_empty()
                && !data.contains(['\r', '\n'])
            {
                output.output.push(InputContent::Image(ImageContent {
                    kind: Default::default(),
                    data: data.to_owned(),
                    mime_type: mime.to_owned(),
                }));
            }
        }
    }
    Ok(())
}

fn image_usage(raw: &Value, model: &ImageModel) -> Usage {
    let prompt = raw["prompt_tokens"].as_f64().unwrap_or(0.0);
    let details = &raw["prompt_tokens_details"];
    let write = details["cache_write_tokens"].as_f64().unwrap_or(0.0);
    let cached = details["cached_tokens"].as_f64().unwrap_or(0.0);
    let read = if write > 0.0 {
        (cached - write).max(0.0)
    } else {
        cached
    };
    let input = (prompt - read - write).max(0.0);
    let output = raw["completion_tokens"].as_f64().unwrap_or(0.0);
    let r = &model.cost.rates;
    let mut cost = Cost {
        input: r.input / 1e6 * input,
        output: r.output / 1e6 * output,
        cache_read: r.cache_read / 1e6 * read,
        cache_write: r.cache_write / 1e6 * write,
        total: 0.0,
        total_from_provider: Some(false),
    };
    cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
    Usage {
        input,
        output,
        cache_read: read,
        cache_write: write,
        total_tokens: input + output + read + write,
        cost,
        ..Usage::default()
    }
}

pub(crate) fn image_data_url(image: &ImageContent) -> String {
    format!("data:{};base64,{}", image.mime_type, image.data)
}

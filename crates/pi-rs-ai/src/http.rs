use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use reqwest::{
    Client, Method, Response, StatusCode,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::{Error, Result};
use crate::model::BaseModel;
use crate::proxy::resolve_http_proxy_url_for_target;
use crate::request::{
    ClientIdentity, ProviderHeaders, ProviderRequestOptions, ProviderResponse, UserAgent,
    merge_headers,
};

const MAX_ERROR_UNITS: usize = 4_000;
pub(crate) const PROVIDER_TIMEOUT_MS: u64 = 600_000;

pub fn format_user_agent(identity: &ClientIdentity) -> String {
    match &identity.user_agent {
        Some(UserAgent::Short) => "pi".to_owned(),
        Some(UserAgent::Custom(custom)) => custom.clone(),
        _ => format!(
            "pi/{} ({}; rust; {})",
            identity.version.as_deref().unwrap_or("1.6.0"),
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    }
}

pub fn identity_headers<A>(model: &BaseModel<A>, identity: &ClientIdentity) -> ProviderHeaders {
    let mut headers = ProviderHeaders::new();
    headers.insert("User-Agent".into(), Some(format_user_agent(identity)));
    let Some(Some(attribution)) = &identity.attribution else {
        return headers;
    };
    let url = Url::parse(&model.base_url).ok();
    let host = url.as_ref().and_then(Url::host_str);
    if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
        headers.insert("HTTP-Referer".into(), Some(attribution.url.clone()));
        headers.insert("X-OpenRouter-Title".into(), Some(attribution.name.clone()));
        if let Some(categories) = attribution.categories.as_ref().filter(|c| !c.is_empty()) {
            headers.insert("X-OpenRouter-Categories".into(), Some(categories.join(",")));
        }
    } else if model.provider == "nvidia" || host == Some("integrate.api.nvidia.com") {
        let name = &attribution.name;
        let mut value = String::with_capacity(name.len());
        if let Some(first) = name.chars().next() {
            value.extend(first.to_uppercase());
            value.push_str(&name[first.len_utf8()..]);
        }
        headers.insert("X-BILLING-INVOKE-ORIGIN".into(), Some(value));
    } else if matches!(model.provider.as_str(), "opencode" | "opencode-go")
        || host == Some("opencode.ai")
    {
        headers.insert("x-opencode-client".into(), Some(attribution.name.clone()));
    }
    headers
}

pub fn header_map(headers: ProviderHeaders) -> Result<HeaderMap> {
    let mut converted = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        if let Some(value) = value {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| Error::Protocol(e.to_string()))?;
            let value =
                HeaderValue::from_str(&value).map_err(|e| Error::Protocol(e.to_string()))?;
            converted.insert(name, value);
        }
    }
    Ok(converted)
}

pub fn request_headers<M, A>(
    model: &BaseModel<A>,
    options: &ProviderRequestOptions<M>,
    defaults: ProviderHeaders,
) -> Result<HeaderMap> {
    let mut headers = merge_headers(identity_headers(model, &options.identity), defaults);
    if let Some(model_headers) = &model.headers {
        headers = merge_headers(
            headers,
            model_headers
                .iter()
                .map(|(k, v)| (k.clone(), Some(v.clone())))
                .collect(),
        );
    }
    header_map(merge_headers(headers, options.headers.clone()))
}

pub fn client_for<M>(
    target: &Url,
    options: &ProviderRequestOptions<M>,
    force_http1: bool,
) -> Result<Client> {
    if let Some(client) = &options.client {
        return Ok(client.clone());
    }
    let proxy = resolve_http_proxy_url_for_target(target.as_str(), &options.env)?;
    if proxy.is_none() && !force_http1 {
        static DEFAULT: OnceLock<std::result::Result<Client, String>> = OnceLock::new();
        return DEFAULT
            .get_or_init(|| build_client(None, false).map_err(|e| e.to_string()))
            .as_ref()
            .cloned()
            .map_err(|e| Error::Protocol(e.clone()));
    }
    build_client(proxy, force_http1)
}

fn build_client(proxy: Option<Url>, force_http1: bool) -> Result<Client> {
    let mut builder = Client::builder().no_proxy().retry(reqwest::retry::never());
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy.as_str())?);
    }
    if force_http1 {
        builder = builder.http1_only();
    }
    Ok(builder.build()?)
}

pub fn truncate_error_text(text: &str, max_units: usize) -> String {
    let mut units = 0;
    let mut end = text.len();
    for (i, ch) in text.char_indices() {
        units += ch.len_utf16();
        if units > max_units {
            end = i;
            break;
        }
    }
    if end == text.len() {
        return text.to_owned();
    }
    let total = units
        + text[end..]
            .chars()
            .skip(1)
            .map(char::len_utf16)
            .sum::<usize>();
    format!(
        "{}... [truncated {} chars]",
        &text[..end],
        total - max_units
    )
}

pub(crate) fn parse_float_prefix(text: &str) -> Option<f64> {
    let text = text.trim_start();
    let len = text
        .bytes()
        .take_while(|b| b.is_ascii_digit() || b"eE.+-".contains(b))
        .count();
    (1..=len)
        .rev()
        .find_map(|end| text[..end].parse::<f64>().ok().filter(|n| n.is_finite()))
}

fn retry_delay(
    error: &Error,
    retry_index: u32,
    cap: u64,
    skip: &[StatusCode],
) -> Result<Option<Duration>> {
    let status_and_headers = match error {
        Error::Http {
            status, headers, ..
        } => Some((*status, headers.as_ref())),
        Error::Transport(error)
            if !error.is_builder() && !error.is_body() && !error.is_decode() =>
        {
            None
        }
        Error::Timeout(_) => None,
        _ => return Ok(None),
    };
    if let Some((status, headers)) = status_and_headers {
        if skip.contains(&status) {
            return Ok(None);
        }
        let header = |name| headers.get(name).and_then(|s| s.to_str().ok());
        let retryable = match header("x-should-retry") {
            Some("true") => true,
            Some("false") => false,
            _ => status.is_server_error() || matches!(status.as_u16(), 408 | 409 | 429),
        };
        if !retryable {
            return Ok(None);
        }
        let requested = header("retry-after-ms")
            .and_then(parse_float_prefix)
            .or_else(|| {
                let text = header("retry-after")?;
                parse_float_prefix(text).map(|s| s * 1000.0).or_else(|| {
                    let date = httpdate::parse_http_date(text).ok()?;
                    let duration = date.duration_since(SystemTime::now()).unwrap_or_default();
                    Some(duration.as_secs_f64() * 1000.0)
                })
            });
        if let Some(requested) = requested {
            if cap > 0 && requested > cap as f64 {
                return Err(Error::Protocol(format!(
                    "Server requested {}s retry delay (max: {}s). {error}",
                    (requested / 1000.0).ceil(),
                    cap.div_ceil(1000)
                )));
            }
            let duration = Duration::try_from_secs_f64(requested.max(0.0) / 1000.0)
                .map_err(|_| Error::Protocol(format!("Invalid server retry delay. {error}")))?;
            return Ok(Some(duration));
        }
    }
    let factor = 1.0 - rand::random::<f64>() * 0.25;
    Ok(Some(Duration::from_secs_f64(
        (0.5 * 2f64.powi(retry_index.min(4) as i32)).min(8.0) * factor,
    )))
}

pub async fn abortable<T>(
    signal: &CancellationToken,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! { biased; _ = signal.cancelled() => Err(Error::Aborted), result = work => result }
}

pub async fn retry<T, M, F, Fut>(
    options: &ProviderRequestOptions<M>,
    default_retries: u32,
    skip: &[StatusCode],
    mut work: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let max = options.max_retries.unwrap_or(default_retries);
    let mut attempted = 0;
    loop {
        let error = match abortable(&options.signal, work()).await {
            Ok(response) => return Ok(response),
            Err(error) if attempted == max => return Err(error),
            Err(error) => error,
        };
        let cap = options.max_retry_delay_ms.unwrap_or(60_000);
        let Some(delay) = retry_delay(&error, attempted, cap, skip)? else {
            return Err(error);
        };
        attempted += 1;
        abortable(&options.signal, async {
            tokio::time::sleep(delay).await;
            Ok(())
        })
        .await?;
    }
}

pub struct HttpRequest {
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    pub body: Option<Vec<u8>>,
    pub error_label: String,
    pub force_http1: bool,
    pub default_retries: u32,
    pub no_retry_statuses: Vec<StatusCode>,
}

pub async fn send<M>(
    request: HttpRequest,
    model: &M,
    options: &ProviderRequestOptions<M>,
) -> Result<Response> {
    let client = client_for(&request.url, options, request.force_http1)?;
    let body = request.body.map(bytes::Bytes::from);
    let duration = Duration::from_millis(options.timeout_ms.unwrap_or(PROVIDER_TIMEOUT_MS));
    let response = retry(
        options,
        request.default_retries,
        &request.no_retry_statuses,
        || async {
            let mut outgoing = client
                .request(request.method.clone(), request.url.clone())
                .headers(request.headers.clone());
            if let Some(bytes) = &body {
                outgoing = outgoing.body(bytes.clone());
            }
            let response = tokio::time::timeout(duration, outgoing.send())
                .await
                .map_err(|_| Error::Timeout(duration))??;
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            let headers = Box::new(response.headers().clone());
            let body = response.text().await?;
            let reason = if body.trim().is_empty() {
                "(no body)".to_owned()
            } else {
                truncate_error_text(body.trim(), MAX_ERROR_UNITS)
            };
            Err(Error::Http {
                status,
                headers,
                message: format!("{} ({}): {reason}", request.error_label, status.as_u16()),
            })
        },
    )
    .await?;
    if let Some(observer) = &options.on_response {
        let metadata = ProviderResponse {
            status: response.status().as_u16(),
            headers: response
                .headers()
                .iter()
                .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_owned())))
                .collect(),
        };
        abortable(&options.signal, observer(&metadata, model)).await?;
    }
    Ok(response)
}

pub async fn payload<M>(
    value: Value,
    model: &M,
    options: &ProviderRequestOptions<M>,
) -> Result<Value> {
    if let Some(transform) = &options.on_payload {
        abortable(&options.signal, transform(value, model)).await
    } else {
        Ok(value)
    }
}

pub fn endpoint(base: &str, suffix: &str) -> Result<Url> {
    let mut url = Url::parse(base)?;
    let path = format!(
        "{}/{}",
        url.path().trim_end_matches('/'),
        suffix.trim_start_matches('/')
    );
    url.set_path(&path);
    Ok(url)
}

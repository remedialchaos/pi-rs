use std::{io, sync::Arc};

use axum::body::{Body, Bytes as BodyBytes};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::stream;
use http::{Method, StatusCode, header};
use pi_rs_chord::{
    Result,
    context::Context,
    contracts::Contract,
    host::{Facet, Held, WeakHost, define_facet},
    node::http::{
        HttpContribution, HttpError, HttpRequest, HttpResponse, HttpResult, RouteOptions,
        json_response, text_response,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    api::v1::{Principal, p2_contracts::lookup},
    runtime::{files::file_preview, http::cross_site_read},
};

use super::environment::{Bytes, CHUNK, LIMIT, valid_name, valid_path};

type Peer = Arc<Contract>;

pub(super) fn facet() -> Result<Facet> {
    let transfer = Arc::new(lookup("p2.workspace-transfer")?);
    let catalog = Arc::new(lookup("p2.catalog")?);
    Ok(define_facet(move |host| {
        let weak = host.host().downgrade();
        let mut routes = HttpContribution::new("p2.workspace-routes");
        for route in [
            "GET /workspace-file",
            "HEAD /workspace-file",
            "POST /workspace-upload",
        ] {
            let (weak, transfer, catalog) = (weak.clone(), transfer.clone(), catalog.clone());
            routes
                .route(
                    route,
                    RouteOptions::new(move |request| {
                        Box::pin(serve(
                            weak.clone(),
                            transfer.clone(),
                            catalog.clone(),
                            request,
                        ))
                    }),
                )
                .map_err(super::invalid)?;
        }
        host.contribute_http(routes)
    }))
}

fn error(status: StatusCode, message: impl ToString) -> HttpResult<HttpResponse> {
    let mut response = json_response(&json!({"error":message.to_string()}))?;
    *response.status_mut() = status;
    Ok(response)
}

async fn serve(
    host: WeakHost,
    transfer: Peer,
    catalog: Peer,
    request: HttpRequest,
) -> HttpResult<HttpResponse> {
    let query = form_urlencoded::parse(request.query.as_bytes());
    let field = |name: &str| {
        query
            .clone()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    };
    let session = field("session").unwrap_or_default();
    let uploading = request.method == Method::POST;
    let principal = request
        .principal
        .as_ref()
        .map(Principal::deserialize)
        .transpose()?;
    if !principal.is_some_and(|principal| principal.can_send(&session))
        || cross_site_read(&request.method, &request.headers)
        || (uploading && request.header("origin") != Some(request.origin.as_str()))
    {
        return text_response(403, "Forbidden");
    }
    let failed = if uploading {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::NOT_FOUND
    };
    let host = host
        .upgrade()
        .ok_or_else(|| HttpError::new(503, "server closed"))?;
    let context = Context::background();
    let result = async {
        let catalog = host.get(catalog, json!({}).into(), &context).await?;
        let row = catalog
            .call("get", json!({"id":session.as_ref()}).into(), &context)
            .await?
            .to_serde()?;
        if row.get("archivedAt").is_some_and(|value| !value.is_null()) {
            return Err(super::invalid("Session is archived"));
        }
        let conversation = field("conversation")
            .as_deref()
            .unwrap_or("1")
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|n| n.fract() == 0. && (1. ..=9_007_199_254_740_991.).contains(n))
            .map(|n| n as u64)
            .ok_or_else(|| super::invalid("Invalid workspace location"))?;
        let path = field("path").unwrap_or_default().into_owned();
        if !valid_path(&path) {
            return Err(super::invalid("Invalid workspace location"));
        }
        let transfer = host
            .get(
                transfer,
                json!({"session":session.as_ref(),"conversation":conversation}).into(),
                &context,
            )
            .await?;
        Ok::<_, pi_rs_chord::Error>((path, transfer))
    }
    .await;
    let (path, transfer) = match result {
        Ok(result) => result,
        Err(cause) => return error(failed, cause),
    };
    if uploading {
        let name = field("name").unwrap_or_default();
        if !valid_name(&name) {
            return error(failed, "Invalid file name");
        }
        let body = match request.body(LIMIT as usize).await {
            Ok(body) => body,
            Err(cause) if cause.status == 413 => return error(failed, "Upload exceeds 25 MiB"),
            Err(cause) => return error(failed, cause.message),
        };
        // A dropped HTTP request must not leave a partially staged upload behind.
        let work = tokio::spawn(upload(transfer, path, name.into_owned(), body));
        match work.await {
            Ok(Ok(value)) => json_response(&value),
            Ok(Err(cause)) => error(failed, cause),
            Err(cause) => error(failed, cause),
        }
    } else {
        download(transfer, path, &request).await
    }
}

async fn upload(transfer: Held, path: String, name: String, body: BodyBytes) -> Result<Value> {
    let ctx = Context::background();
    let token = uuid::Uuid::new_v4().to_string();
    let mut offset = 0usize;
    let result = async {
        for chunk in body.chunks(CHUNK as usize) {
            transfer.call("write",json!({"path":path,"name":name,"token":token,"offset":offset,"data":STANDARD.encode(chunk),"final":false}).into(),&ctx).await?;
            offset += chunk.len();
        }
        transfer.call("write",json!({"path":path,"name":name,"token":token,"offset":offset,"data":"","final":true}).into(),&ctx).await?;
        Ok(json!({"path":if path.is_empty() { name.clone() } else { format!("{path}/{name}") },"size":offset}))
    }.await;
    if result.is_err() {
        let _ = transfer.call("write",json!({"path":path,"name":name,"token":token,"offset":offset,"data":"","final":false,"abort":true}).into(),&ctx).await;
    }
    result
}

fn range(value: &str) -> Option<serde_json::Map<String, Value>> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    let number = |s: &str| {
        if !s.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        s.parse::<u64>()
            .ok()
            .filter(|n| *n <= 9_007_199_254_740_991)
    };
    let mut bounds = serde_json::Map::new();
    if start.is_empty() {
        let suffix = number(end).filter(|v| *v > 0)?;
        bounds.insert("suffix".into(), json!(suffix));
    } else {
        let start = number(start)?;
        bounds.insert("start".into(), json!(start));
        if !end.is_empty() {
            let end = number(end).filter(|end| *end >= start)?;
            bounds.insert("end".into(), json!(end));
        }
    }
    Some(bounds)
}

async fn get_bytes(transfer: &Held, query: Value) -> Result<Bytes> {
    let response = transfer
        .call("read", query.into(), &Context::background())
        .await?;
    Ok(serde_json::from_value(response.to_serde()?)?)
}

async fn download(transfer: Held, path: String, request: &HttpRequest) -> HttpResult<HttpResponse> {
    if path.is_empty() {
        return error(StatusCode::NOT_FOUND, "Invalid file path");
    }
    let name = path.rsplit('/').next().unwrap_or(&path);
    let preview = file_preview(name);
    let media = preview
        .as_ref()
        .is_some_and(|file| matches!(file.kind, "audio" | "video"));
    let header_range = media.then(|| request.header("range")).flatten();
    let mut bounds = match header_range {
        Some(value) => match range(value) {
            Some(bounds) => bounds,
            None => {
                return http::Response::builder()
                    .status(416)
                    .body(Body::empty())
                    .map_err(|e| HttpError::new(500, e.to_string()));
            }
        },
        None => serde_json::Map::new(),
    };
    let partial = header_range.is_some();
    let requested_end = bounds.get("end").and_then(Value::as_u64);
    bounds.insert("path".into(), json!(path));
    let first = match get_bytes(&transfer, Value::Object(bounds)).await {
        Ok(bytes) => bytes,
        Err(cause) => return error(StatusCode::NOT_FOUND, cause),
    };
    let size = first.size;
    let start = first.start;
    let end = requested_end
        .unwrap_or(u64::MAX)
        .min(size.saturating_sub(1));
    if partial && (start >= size || end < start) {
        return http::Response::builder()
            .status(416)
            .header(header::CONTENT_RANGE, format!("bytes */{size}"))
            .body(Body::empty())
            .map_err(|e| HttpError::new(500, e.to_string()));
    }
    if !partial && size > LIMIT {
        return text_response(413, "File too large");
    }
    let mime = preview
        .filter(|file| media || file.kind == "image")
        .map_or("application/octet-stream", |file| file.mime_type);
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded = percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC);
    let mut response = http::Response::builder()
        .status(if partial { 206 } else { 200 })
        .header(header::CONTENT_TYPE, mime)
        .header(
            header::CONTENT_LENGTH,
            if size == 0 { 0 } else { end - start + 1 },
        )
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{safe}\"; filename*=UTF-8''{encoded}"),
        )
        .header(header::ACCEPT_RANGES, if media { "bytes" } else { "none" })
        .header(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; sandbox",
        )
        .header("cross-origin-resource-policy", "same-origin");
    if partial {
        response = response.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    let body = if request.method == Method::HEAD || size == 0 {
        Body::empty()
    } else {
        Body::from_stream(stream::try_unfold(
            (transfer, path, Some(first), start),
            move |(transfer, path, first, offset)| async move {
                if offset > end {
                    return Ok::<_, io::Error>(None);
                }
                let chunk = if let Some(first) = first {
                    first
                } else {
                    get_bytes(&transfer, json!({"path":path,"start":offset,"end":end}))
                        .await
                        .map_err(|e| io::Error::other(e.to_string()))?
                };
                let bytes = STANDARD.decode(chunk.data).map_err(io::Error::other)?;
                if chunk.start != offset
                    || bytes.is_empty()
                    || bytes.len() as u64 > end - offset + 1
                {
                    return Err(io::Error::other("File changed during transfer"));
                }
                let next = offset + bytes.len() as u64;
                Ok(Some((BodyBytes::from(bytes), (transfer, path, None, next))))
            },
        ))
    };
    response
        .body(body)
        .map_err(|e| HttpError::new(500, e.to_string()))
}

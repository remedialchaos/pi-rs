//! `files` built-in plugin: session uploads & download snapshots (`p2.download-store`,
//! `p2.session-files`, `p2.attachments`) and HTTP `/upload` & `/download` helpers.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::body::Body;
use http::{HeaderValue, Method, Response, StatusCode, header};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use pi_rs_chord::{
    Error, Result,
    context::Context,
    data::ChordError,
    host::{Host, Members, ServeOptions, WeakHost, define_facet},
    json::JsonValue,
    node::http::{
        HttpContribution, HttpRequest, HttpResponse, RouteOptions, json_response, text_response,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    api::v1::{
        FacetKind, PluginRegistry, Principal, p2_contracts::lookup, server::service_of,
        worker::worker_of,
    },
    plugins::send_file::WORKER_FILES_SERVICE,
    runtime::{
        files::{file_preview, safe_file_name},
        http::{cross_site_read, same_origin},
        session_files::{
            DOWNLOADS, FILE_CHUNK, FileInfo, FileWrite, Files, MAX_DOWNLOAD_BYTES, UPLOADS,
            display_name, find_download, read_whole,
        },
    },
};

/// Maximum upload size in bytes (25 MiB).
pub const MAX_UPLOAD_BYTES: u64 = 25 * 1024 * 1024;
/// Maximum attachments resolved per call.
pub const MAX_ATTACHMENTS: usize = 32;

/// Returns the inline image MIME type for a filename, or `None` for non-images.
pub fn image_type(name: &str) -> Option<&'static str> {
    match name.rsplit('.').next()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Upload path inside the `uploads` space and materialized directory: `<id>-<safe name>`.
pub fn upload_path(id: &str, name: &str) -> String {
    format!("{id}-{}", safe_file_name(name))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteRange {
    Valid { start: u64, end: u64 },
    Invalid,
}

/// Parses an HTTP `Range: bytes=start-end` header for a file of `size` bytes.
pub fn parse_byte_range(header: Option<&str>, size: u64) -> Option<ByteRange> {
    let header = header?;
    if size == 0 {
        return None;
    }
    let rest = header.strip_prefix("bytes=")?;
    let (start_str, end_str) = match rest.split_once('-') {
        Some(pair) => pair,
        None => return Some(ByteRange::Invalid),
    };
    if !start_str.bytes().all(|b| b.is_ascii_digit())
        || !end_str.bytes().all(|b| b.is_ascii_digit())
    {
        return Some(ByteRange::Invalid);
    }
    let (start, end) = match (start_str.is_empty(), end_str.is_empty()) {
        (true, true) => return Some(ByteRange::Invalid),
        (true, false) => {
            let suffix: u64 = match end_str.parse() {
                Ok(v) => v,
                Err(_) => return Some(ByteRange::Invalid),
            };
            (size.saturating_sub(suffix), size - 1)
        }
        (false, true) => {
            let s: u64 = match start_str.parse() {
                Ok(v) => v,
                Err(_) => return Some(ByteRange::Invalid),
            };
            (s, size - 1)
        }
        (false, false) => {
            let s: u64 = match start_str.parse() {
                Ok(v) => v,
                Err(_) => return Some(ByteRange::Invalid),
            };
            let e: u64 = match end_str.parse() {
                Ok(v) => v,
                Err(_) => return Some(ByteRange::Invalid),
            };
            (s, e)
        }
    };
    if end < start || start >= size {
        return Some(ByteRange::Invalid);
    }
    Some(ByteRange::Valid {
        start,
        end: end.min(size - 1),
    })
}

/// Materializes an upload into `directory` via atomic staging + rename.
pub fn materialize_locally(files: &Files, file: &FileInfo, directory: &Path) -> Result<PathBuf> {
    let target = directory.join(&file.path);
    if let Ok(meta) = std::fs::metadata(&target)
        && meta.is_file()
        && meta.len() == file.size
    {
        return Ok(target);
    }
    std::fs::create_dir_all(directory).map_err(|e| Error::Invalid(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700));
    }
    let staging = directory.join(format!("{}.{}.partial", file.path, std::process::id()));
    let bytes = read_whole(files, file).map_err(|e| Error::Invalid(e.to_string()))?;
    if let Err(err) = std::fs::write(&staging, &bytes) {
        let _ = std::fs::remove_file(&staging);
        return Err(Error::Invalid(err.to_string()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o600));
    }
    if let Err(err) = std::fs::rename(&staging, &target) {
        let _ = std::fs::remove_file(&staging);
        return Err(Error::Invalid(err.to_string()));
    }
    Ok(target)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub path: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
}

#[derive(Deserialize)]
struct SaveArgs {
    session: String,
    path: String,
    name: Option<String>,
}

#[derive(Deserialize)]
struct ResolveArgs {
    ids: Vec<String>,
    #[expect(
        dead_code,
        reason = "deserialized to validate required wire argument on p2.attachments resolve"
    )]
    conversation: u64,
}

#[derive(Deserialize)]
struct BeginArgs {
    space: String,
    name: String,
    source: String,
}

#[derive(Deserialize)]
struct AppendArgs {
    upload: String,
    offset: u64,
    bytes: Vec<u8>,
}

#[derive(Deserialize)]
struct ReadArgs {
    file: String,
    offset: u64,
    length: usize,
}

const RFC5987_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

fn query_param(query: &str, key: &str) -> String {
    form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

fn upload_error_response(status: u16, message: &str) -> HttpResponse {
    let mut response = json_response(&json!({
        "error": message,
        "limit": MAX_UPLOAD_BYTES,
    }))
    .unwrap_or_else(|_| HttpResponse::new(Body::from("{}")));
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
    response
}

fn status_text(status: StatusCode, body: &'static str) -> HttpResponse {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    response
}

async fn handle_upload(
    request: HttpRequest,
    host: &Host,
    session_files: &pi_rs_chord::contracts::Contract,
    catalog: &pi_rs_chord::contracts::Contract,
) -> HttpResponse {
    if !same_origin(&request.headers, &request.origin) {
        return upload_error_response(403, "Invalid origin");
    }
    let session = query_param(&request.query, "session");
    let principal = request
        .principal
        .as_ref()
        .and_then(|v| Principal::deserialize(v).ok());
    if !principal.as_ref().is_some_and(|p| p.can_send(&session)) {
        return upload_error_response(403, "Forbidden");
    }
    let name = query_param(&request.query, "name");
    if name.is_empty() || name.encode_utf16().count() > 255 {
        return upload_error_response(400, "Invalid file name");
    }
    if let Some(len) = request
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok())
        && len > MAX_UPLOAD_BYTES
    {
        return upload_error_response(413, "File too large");
    }
    let bytes = match request.body(MAX_UPLOAD_BYTES as usize).await {
        Ok(bytes) => bytes,
        Err(err) if err.status == 413 => return upload_error_response(413, "File too large"),
        Err(err) => return upload_error_response(err.status, &err.message),
    };
    if bytes.len() as u64 > MAX_UPLOAD_BYTES {
        return upload_error_response(413, "File too large");
    }
    let source = match principal {
        Some(Principal::Guest { id, .. }) => format!("guest:{id}"),
        _ => "owner".to_owned(),
    };
    let root = host.root();
    let ctx = Context::background();
    let cat = match root.hold(Arc::new(catalog.clone()), JsonValue::from_serde(json!({}))) {
        Ok(held) => held,
        Err(err) => return upload_error_response(400, &err.to_string()),
    };
    if let Err(err) = cat.wait(&ctx).await {
        return upload_error_response(400, &err.to_string());
    }
    let row = match cat
        .call("get", JsonValue::from_serde(json!({ "id": session })), &ctx)
        .await
    {
        Ok(val) => match val.to_serde() {
            Ok(v) => v,
            Err(err) => return upload_error_response(400, &err.to_string()),
        },
        Err(_) => return upload_error_response(400, "Unknown session"),
    };
    if !row["archivedAt"].is_null() {
        return upload_error_response(409, "Session is archived");
    }
    let server = match crate::runtime::server::server_of(host) {
        Ok(server) => server,
        Err(_) => return upload_error_response(503, "Server unavailable"),
    };
    let files = match server
        .workers
        .hold(
            Arc::new(session_files.clone()),
            JsonValue::from_serde(json!({ "session": session })),
            pi_rs_chord::host::Caller {
                principal: Some(json!({ "kind": "server" })),
                incarnation: host.incarnation().into(),
            },
            &ctx,
        )
        .await
    {
        Ok(held) => held,
        Err(err) => return upload_error_response(400, &err.to_string()),
    };
    let outcome = async {
        let begun = match files
            .call(
                "begin",
                JsonValue::from_serde(json!({
                    "space": UPLOADS,
                    "name": name,
                    "source": source,
                })),
                &ctx,
            )
            .await
            .and_then(|v| v.to_serde())
        {
            Ok(v) => v,
            Err(err) => return upload_error_response(400, &err.to_string()),
        };
        let Some(upload) = begun["upload"].as_str().map(str::to_owned) else {
            return upload_error_response(400, "Missing upload token");
        };
        let mut offset = 0usize;
        for chunk in bytes.chunks(FILE_CHUNK) {
            if let Err(err) = files
                .call(
                    "append",
                    JsonValue::from_serde(json!({
                        "upload": upload,
                        "offset": offset,
                        "bytes": chunk,
                    })),
                    &ctx,
                )
                .await
            {
                let _ = files
                    .call("cancel", JsonValue::from_serde(Value::String(upload)), &ctx)
                    .await;
                return upload_error_response(400, &err.to_string());
            }
            offset += chunk.len();
        }
        match files
            .call("finish", JsonValue::from_serde(Value::String(upload)), &ctx)
            .await
            .and_then(|v| v.to_serde())
        {
            Ok(saved) => {
                json_response(&saved).unwrap_or_else(|_| status_text(StatusCode::OK, "{}"))
            }
            Err(err) => upload_error_response(400, &err.to_string()),
        }
    }
    .await;
    files.release();
    outcome
}

async fn handle_download(
    request: HttpRequest,
    host: &Host,
    session_files: &pi_rs_chord::contracts::Contract,
    catalog: &pi_rs_chord::contracts::Contract,
) -> HttpResponse {
    let session = query_param(&request.query, "session");
    let principal = request
        .principal
        .as_ref()
        .and_then(|v| Principal::deserialize(v).ok());
    if cross_site_read(&request.method, &request.headers)
        || !principal.as_ref().is_some_and(|p| p.can_view(&session))
    {
        return status_text(StatusCode::FORBIDDEN, "Forbidden");
    }
    let id = query_param(&request.query, "id");
    let root = host.root();
    let ctx = Context::background();
    let Ok(cat) = root.hold(Arc::new(catalog.clone()), JsonValue::from_serde(json!({}))) else {
        return status_text(StatusCode::NOT_FOUND, "File not found");
    };
    if cat.wait(&ctx).await.is_err() {
        return status_text(StatusCode::NOT_FOUND, "File not found");
    }
    let Ok(row) = cat
        .call("get", JsonValue::from_serde(json!({ "id": session })), &ctx)
        .await
        .and_then(|v| v.to_serde())
    else {
        return status_text(StatusCode::NOT_FOUND, "File not found");
    };
    if !row["archivedAt"].is_null() {
        return status_text(StatusCode::NOT_FOUND, "File not found");
    }
    let Ok(server) = crate::runtime::server::server_of(host) else {
        return status_text(StatusCode::NOT_FOUND, "File not found");
    };
    let Ok(files) = server
        .workers
        .hold(
            Arc::new(session_files.clone()),
            JsonValue::from_serde(json!({ "session": session })),
            pi_rs_chord::host::Caller {
                principal: Some(json!({ "kind": "server" })),
                incarnation: host.incarnation().into(),
            },
            &ctx,
        )
        .await
    else {
        return status_text(StatusCode::NOT_FOUND, "File not found");
    };
    let outcome = async {
        let Ok(found) = files
            .call("download", JsonValue::from_serde(Value::String(id)), &ctx)
            .await
            .and_then(|v| v.to_serde())
        else {
            return status_text(StatusCode::NOT_FOUND, "File not found");
        };
        if found.is_null() {
            return status_text(StatusCode::NOT_FOUND, "File not found");
        }
        let Some(file_id) = found["file"].as_str().map(str::to_owned) else {
            return status_text(StatusCode::NOT_FOUND, "File not found");
        };
        let name = found["name"].as_str().unwrap_or("file").to_owned();
        let size = found["size"].as_u64().unwrap_or(0);
        let preview = file_preview(&name);
        let seekable = preview
            .as_ref()
            .is_some_and(|p| matches!(p.kind, "audio" | "video"));
        let range = if seekable {
            parse_byte_range(request.header("range"), size)
        } else {
            None
        };
        if matches!(range, Some(ByteRange::Invalid)) {
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            if let Ok(val) = HeaderValue::from_str(&format!("bytes */{size}")) {
                resp.headers_mut().insert(header::CONTENT_RANGE, val);
            }
            return resp;
        }
        let (start, end, is_partial) = match range {
            Some(ByteRange::Valid { start, end }) => (start, end, true),
            _ => (0, size.saturating_sub(1), false),
        };
        let content_length = if size == 0 { 0 } else { end - start + 1 };
        let ascii: String = name
            .chars()
            .map(|c| {
                if (' '..='~').contains(&c) && c != '"' && c != '\\' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let encoded_name = utf8_percent_encode(&name, RFC5987_SET).to_string();
        let disposition =
            format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded_name}");

        let body = if request.method == Method::HEAD || content_length == 0 {
            Body::empty()
        } else {
            let mut out = Vec::with_capacity(content_length as usize);
            let mut cursor = start;
            while cursor <= end {
                let remaining = (end - cursor + 1).min(FILE_CHUNK as u64) as usize;
                let Ok(chunk_val) = files
                    .call(
                        "read",
                        JsonValue::from_serde(json!({
                            "file": file_id,
                            "offset": cursor,
                            "length": remaining,
                        })),
                        &ctx,
                    )
                    .await
                    .and_then(|v| v.to_serde())
                else {
                    return status_text(StatusCode::INTERNAL_SERVER_ERROR, "Could not read file");
                };
                let Ok(chunk) = serde_json::from_value::<Vec<u8>>(chunk_val) else {
                    return status_text(StatusCode::INTERNAL_SERVER_ERROR, "Could not read file");
                };
                if chunk.is_empty() {
                    break;
                }
                cursor += chunk.len() as u64;
                out.extend_from_slice(&chunk);
            }
            Body::from(out)
        };

        let mut response = Response::new(body);
        *response.status_mut() = if is_partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        };
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(preview.map_or("application/octet-stream", |p| p.mime_type)),
        );
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(content_length));
        if let Ok(val) = HeaderValue::from_str(&disposition) {
            headers.insert(header::CONTENT_DISPOSITION, val);
        }
        headers.insert(
            header::ACCEPT_RANGES,
            HeaderValue::from_static(if seekable { "bytes" } else { "none" }),
        );
        headers.insert(
            header::HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        );
        if is_partial && let Ok(val) = HeaderValue::from_str(&format!("bytes {start}-{end}/{size}"))
        {
            headers.insert(header::CONTENT_RANGE, val);
        }
        response
    }
    .await;
    files.release();
    outcome
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let download_store_contract = Arc::new(lookup("p2.downloads")?);
    let session_files_contract = Arc::new(lookup("p2.session-files")?);
    let attachments_contract = Arc::new(lookup("p2.attachments")?);
    let catalog_contract = Arc::new(lookup("p2.catalog")?);

    // Server facet: `p2.download-store` + HTTP `/upload` & `/download`
    {
        let sf_ref = session_files_contract.clone();
        let cat_ref = catalog_contract.clone();
        registry.register(
            "files",
            FacetKind::Server,
            define_facet(move |host| {
                let mut http = HttpContribution::new("files");
                {
                    let weak: WeakHost = host.host().downgrade();
                    let sf_http = sf_ref.clone();
                    let cat_http = cat_ref.clone();
                    http.route(
                        "POST /upload",
                        RouteOptions::public(move |request| {
                            let weak = weak.clone();
                            let sf_http = sf_http.clone();
                            let cat_http = cat_http.clone();
                            Box::pin(async move {
                                let Some(host) = weak.upgrade() else {
                                    return text_response(503, "Server closed");
                                };
                                Ok(handle_upload(request, &host, &sf_http, &cat_http).await)
                            })
                        }),
                    )
                    .map_err(|err| Error::Invalid(err.to_string()))?;
                }
                for method in ["GET", "HEAD"] {
                    let weak: WeakHost = host.host().downgrade();
                    let sf_http = sf_ref.clone();
                    let cat_http = cat_ref.clone();
                    http.route(
                        &format!("{method} /download"),
                        RouteOptions::public(move |request| {
                            let weak = weak.clone();
                            let sf_http = sf_http.clone();
                            let cat_http = cat_http.clone();
                            Box::pin(async move {
                                let Some(host) = weak.upgrade() else {
                                    return text_response(503, "Server closed");
                                };
                                Ok(handle_download(request, &host, &sf_http, &cat_http).await)
                            })
                        }),
                    )
                    .map_err(|err| Error::Invalid(err.to_string()))?;
                }
                host.contribute_http(http)?;

                let sf_ref = sf_ref.clone();
                let cat_ref = cat_ref.clone();
                host.serve(
                    download_store_contract.clone(),
                    move |_, _, object| {
                        let catalog = object
                            .host()
                            .served_contract("p2.catalog")
                            .map(|_| object.get(cat_ref.clone(), JsonValue::from_serde(json!({}))))
                            .transpose()?;
                        let sf_contract = sf_ref.clone();
                        let host_handle = object.host();
                        let mut members = Members::new();
                        members.method("save", move |args, ctx| {
                            let catalog = catalog.clone();
                            let sf_contract = sf_contract.clone();
                            let host_handle = host_handle.clone();
                            Box::pin(async move {
                                let parsed: SaveArgs = serde_json::from_value(args.to_serde()?)?;
                                if let Some(cat) = catalog {
                                    let row = cat
                                        .call(
                                            "get",
                                            JsonValue::from_serde(json!({ "id": parsed.session })),
                                            &ctx.context,
                                        )
                                        .await?
                                        .to_serde()?;
                                    if row.get("archivedAt").is_some_and(|v| !v.is_null()) {
                                        return Err(Error::Invalid("Session is archived".into()));
                                    }
                                }
                                let meta = std::fs::metadata(&parsed.path).map_err(|_| {
                                    Error::Invalid("Could not open source file".into())
                                })?;
                                if !meta.is_file() {
                                    return Err(Error::Invalid(
                                        "Source must be a regular file".into(),
                                    ));
                                }
                                if meta.len() > MAX_DOWNLOAD_BYTES {
                                    return Err(Error::Invalid("File exceeds 25 MiB".into()));
                                }
                                let bytes = std::fs::read(&parsed.path).map_err(|_| {
                                    Error::Invalid("Could not open source file".into())
                                })?;
                                if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
                                    return Err(Error::Invalid("File exceeds 25 MiB".into()));
                                }
                                let name = parsed.name.unwrap_or_else(|| {
                                    Path::new(&parsed.path)
                                        .file_name()
                                        .and_then(|s| s.to_str())
                                        .unwrap_or("file")
                                        .to_owned()
                                });
                                let files_held = host_handle
                                    .get(
                                        sf_contract,
                                        JsonValue::from_serde(json!({ "session": parsed.session })),
                                        &ctx.context,
                                    )
                                    .await?;
                                let begun = files_held
                                    .call(
                                        "begin",
                                        JsonValue::from_serde(json!({
                                            "space": DOWNLOADS,
                                            "name": name,
                                            "source": "server",
                                        })),
                                        &ctx.context,
                                    )
                                    .await?
                                    .to_serde()?;
                                let upload = begun["upload"]
                                    .as_str()
                                    .ok_or_else(|| Error::Invalid("missing upload token".into()))?
                                    .to_owned();
                                let mut offset = 0usize;
                                for chunk in bytes.chunks(FILE_CHUNK) {
                                    if let Err(err) = files_held
                                        .call(
                                            "append",
                                            JsonValue::from_serde(json!({
                                                "upload": upload,
                                                "offset": offset,
                                                "bytes": chunk,
                                            })),
                                            &ctx.context,
                                        )
                                        .await
                                    {
                                        let _ = files_held
                                            .call(
                                                "cancel",
                                                JsonValue::from_serde(Value::String(upload)),
                                                &ctx.context,
                                            )
                                            .await;
                                        return Err(err);
                                    }
                                    offset += chunk.len();
                                }
                                let saved = files_held
                                    .call(
                                        "finish",
                                        JsonValue::from_serde(Value::String(upload)),
                                        &ctx.context,
                                    )
                                    .await?
                                    .to_serde()?;
                                Ok(JsonValue::from_serde(json!({
                                    "id": saved["id"],
                                    "name": saved["name"],
                                    "size": saved["size"],
                                })))
                            })
                        })?;
                        Ok(members)
                    },
                    ServeOptions::default(),
                )
            }),
        )?;
    }

    // Worker facet: `p2.attachments` and `p2.session-files`
    registry.register(
        "files",
        FacetKind::Worker,
        define_facet(move |host| {
            host.serve(
                attachments_contract.clone(),
                move |_, _, object| {
                    let host_handle = object.host();
                    let mut members = Members::new();
                    members.method("resolve", move |args, _| {
                        let host_handle = host_handle.clone();
                        Box::pin(async move {
                            let parsed: ResolveArgs = serde_json::from_value(args.to_serde()?)?;
                            if parsed.ids.is_empty() {
                                return Ok(JsonValue::from_serde(json!([])));
                            }
                            let files = service_of::<Files>(&host_handle, WORKER_FILES_SERVICE)
                                .map_err(|_| {
                                    Error::Invalid("Session files store unavailable".into())
                                })?;
                            let (data_dir, session_id) = match worker_of(&host_handle) {
                                Ok(w) => (
                                    PathBuf::from(&w.init.data_dir),
                                    w.init.values["session"]["id"]
                                        .as_str()
                                        .unwrap_or("session")
                                        .to_owned(),
                                ),
                                Err(_) => (std::env::temp_dir(), "session".to_owned()),
                            };
                            let local_dir = data_dir.join("uploads").join(&session_id);
                            let mut out = Vec::with_capacity(parsed.ids.len());
                            for id in parsed.ids {
                                let file = files
                                    .by_key(UPLOADS, &id)
                                    .ok_or_else(|| Error::Invalid("Unknown attachment".into()))?;
                                let materialized = materialize_locally(&files, &file, &local_dir)?;
                                let name = display_name(&file).to_owned();
                                let mime_type = image_type(&name).map(str::to_owned);
                                out.push(Attachment {
                                    id,
                                    name,
                                    path: materialized.to_string_lossy().into_owned(),
                                    size: file.size,
                                    mime_type,
                                    local_path: None,
                                });
                            }
                            Ok(JsonValue::from_serde(serde_json::to_value(out)?))
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )?;

            host.serve(
                session_files_contract.clone(),
                move |_, principal, object| {
                    let parsed = principal
                        .and_then(|v| Principal::deserialize(v).ok())
                        .ok_or_else(|| ChordError::new("forbidden", "server only"))?;
                    if !matches!(parsed, Principal::Server) {
                        return Err(ChordError::new("forbidden", "server only").into());
                    }
                    let host_handle = object.host();
                    let mut members = Members::variant("server");

                    {
                        let hh = host_handle.clone();
                        members.method("begin", move |args, _| {
                            let hh = hh.clone();
                            Box::pin(async move {
                                let parsed: BeginArgs = serde_json::from_value(args.to_serde()?)?;
                                let files = service_of::<Files>(&hh, WORKER_FILES_SERVICE)
                                    .map_err(|_| {
                                        Error::Invalid("Session files store unavailable".into())
                                    })?;
                                let id = uuid::Uuid::new_v4().to_string();
                                let path = upload_path(&id, &parsed.name);
                                let display = path.get(id.len() + 1..).unwrap_or(&path).to_owned();
                                let mime = image_type(&path).unwrap_or("application/octet-stream");
                                let mut write = FileWrite::new(&parsed.space, &path);
                                write.mime = mime.into();
                                write.source = parsed.source;
                                write.key = Some(id.clone());
                                write.meta = Some(json!({ "name": display }));
                                let upload = files.begin(write)?;
                                Ok(JsonValue::from_serde(json!({
                                    "id": id,
                                    "upload": upload,
                                })))
                            })
                        })?;
                    }

                    {
                        let hh = host_handle.clone();
                        members.method("append", move |args, _| {
                            let hh = hh.clone();
                            Box::pin(async move {
                                let parsed: AppendArgs = serde_json::from_value(args.to_serde()?)?;
                                if parsed.offset + parsed.bytes.len() as u64 > MAX_UPLOAD_BYTES {
                                    return Err(Error::Invalid("File too large".into()));
                                }
                                let files = service_of::<Files>(&hh, WORKER_FILES_SERVICE)
                                    .map_err(|_| {
                                        Error::Invalid("Session files store unavailable".into())
                                    })?;
                                files
                                    .append(&parsed.upload, parsed.offset, &parsed.bytes)
                                    .await?;
                                Ok(JsonValue::Null)
                            })
                        })?;
                    }

                    {
                        let hh = host_handle.clone();
                        members.method("finish", move |args, _| {
                            let hh = hh.clone();
                            Box::pin(async move {
                                let upload: String = serde_json::from_value(args.to_serde()?)?;
                                let files = service_of::<Files>(&hh, WORKER_FILES_SERVICE)
                                    .map_err(|_| {
                                        Error::Invalid("Session files store unavailable".into())
                                    })?;
                                let file = files.finish(&upload).await?;
                                let id = file.key.clone().unwrap_or_else(|| file.id.clone());
                                let name = display_name(&file).to_owned();
                                let mime_type = image_type(&name);
                                let mut out = json!({
                                    "id": id,
                                    "name": name,
                                    "size": file.size,
                                });
                                if let Some(mime) = mime_type {
                                    out["mimeType"] = json!(mime);
                                }
                                Ok(JsonValue::from_serde(out))
                            })
                        })?;
                    }

                    {
                        let hh = host_handle.clone();
                        members.method("cancel", move |args, _| {
                            let hh = hh.clone();
                            Box::pin(async move {
                                let upload: String = serde_json::from_value(args.to_serde()?)?;
                                let files = service_of::<Files>(&hh, WORKER_FILES_SERVICE)
                                    .map_err(|_| {
                                        Error::Invalid("Session files store unavailable".into())
                                    })?;
                                files.cancel(&upload)?;
                                Ok(JsonValue::Null)
                            })
                        })?;
                    }

                    {
                        let hh = host_handle.clone();
                        members.method("download", move |args, _| {
                            let hh = hh.clone();
                            Box::pin(async move {
                                let id: String = serde_json::from_value(args.to_serde()?)?;
                                let files = service_of::<Files>(&hh, WORKER_FILES_SERVICE)
                                    .map_err(|_| {
                                        Error::Invalid("Session files store unavailable".into())
                                    })?;
                                match find_download(&files, &id) {
                                    None => Ok(JsonValue::Null),
                                    Some((file, name)) => Ok(JsonValue::from_serde(json!({
                                        "file": file.id,
                                        "name": name,
                                        "size": file.size,
                                    }))),
                                }
                            })
                        })?;
                    }

                    {
                        let hh = host_handle;
                        members.method("read", move |args, _| {
                            let hh = hh.clone();
                            Box::pin(async move {
                                let parsed: ReadArgs = serde_json::from_value(args.to_serde()?)?;
                                let files = service_of::<Files>(&hh, WORKER_FILES_SERVICE)
                                    .map_err(|_| {
                                        Error::Invalid("Session files store unavailable".into())
                                    })?;
                                let info = files
                                    .file(&parsed.file)
                                    .ok_or_else(|| Error::Invalid("File not found".into()))?;
                                if info.space != DOWNLOADS {
                                    return Err(Error::Invalid("File not found".into()));
                                }
                                let bytes =
                                    files.read(&parsed.file, parsed.offset, parsed.length)?;
                                Ok(JsonValue::from_serde(serde_json::to_value(bytes)?))
                            })
                        })?;
                    }

                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )
}

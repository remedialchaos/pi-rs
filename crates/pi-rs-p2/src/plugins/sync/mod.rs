//! `sync` built-in plugin: Radius HTTP adapter for Chord's session-sync remote contract
//! (`apps/p2/plugins/sync/radius-remote.ts`).

use std::sync::Arc;

use pi_rs_chord::{context::Context, sync::SyncFuture};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Sync protocol version sent in `pi-sync-protocol` header on every Radius sync request.
pub const SYNC_PROTOCOL: u32 = 1;

const PAGE_SIZE: usize = 100;
const PROTOCOL_HEADER: &str = "pi-sync-protocol";
const PROTOCOL_TOO_OLD: &str = "protocol_too_old";

const URI_COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

fn encode_segment(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, URI_COMPONENT).to_string()
}

/// Authentication & routing credentials for a Radius sync request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RadiusSyncAccess {
    pub gateway: String,
    pub server_id: String,
    pub token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PutBatchResult {
    Created,
    Exists { to: u64, device: String },
    NotHead { head: u64 },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteListing {
    pub batches: Vec<Value>,
    pub snapshots: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionChange {
    pub id: String,
    pub head: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionChanges {
    pub sessions: Vec<SessionChange>,
    pub cursor: String,
    pub more: bool,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct RadiusSyncError {
    pub message: String,
    pub status: u16,
    pub code: Option<String>,
}

impl RadiusSyncError {
    pub fn new(message: impl Into<String>, status: u16, code: Option<String>) -> Self {
        Self {
            message: message.into(),
            status,
            code,
        }
    }
}

/// Returns true when retrying `error` cannot help (400, 403, 413, or 409 `protocol_too_old`).
pub fn is_terminal_sync_error(error: &RadiusSyncError) -> bool {
    matches!(error.status, 400 | 403 | 413) || error.code.as_deref() == Some(PROTOCOL_TOO_OLD)
}

pub type AccessProvider = Arc<dyn Fn() -> SyncFuture<RadiusSyncAccess> + Send + Sync>;

/// HTTP adapter for Chord's session-sync remote contract over Radius's server-scoped API.
#[derive(Clone)]
pub struct RadiusSyncRemote {
    access: AccessProvider,
    client: reqwest::Client,
}

impl RadiusSyncRemote {
    pub fn new(access: AccessProvider) -> Self {
        Self {
            access,
            client: reqwest::Client::new(),
        }
    }

    pub fn with_client(access: AccessProvider, client: reqwest::Client) -> Self {
        Self { access, client }
    }

    async fn build_url(
        &self,
        path: &str,
    ) -> std::result::Result<(String, String), RadiusSyncError> {
        let access = (self.access)().await.map_err(|e| {
            RadiusSyncError::new(format!("Radius sync access failed: {e}"), 0, None)
        })?;
        let base = access.gateway.trim_end_matches('/');
        let server_id = encode_segment(&access.server_id);
        let url = format!("{base}/v1/servers/{server_id}/sync/sessions{path}");
        Ok((url, access.token))
    }

    async fn send_request(
        &self,
        method: reqwest::Method,
        path: &str,
        content_type: Option<&str>,
        body: Option<Vec<u8>>,
        context: &Context,
    ) -> std::result::Result<reqwest::Response, RadiusSyncError> {
        if context.is_aborted() {
            return Err(RadiusSyncError::new("Radius sync request aborted", 0, None));
        }
        let (url, token) = self.build_url(path).await?;
        let mut req = self
            .client
            .request(method, &url)
            .header("authorization", format!("Bearer {token}"))
            .header(PROTOCOL_HEADER, SYNC_PROTOCOL.to_string());
        if let Some(ct) = content_type {
            req = req.header("content-type", ct);
        }
        if let Some(bytes) = body {
            req = req.body(bytes);
        }
        context
            .await_with(req.send())
            .await
            .map_err(|_| RadiusSyncError::new("Radius sync request aborted", 0, None))?
            .map_err(|e| {
                RadiusSyncError::new(format!("Radius sync transport failed: {e}"), 0, None)
            })
    }

    pub async fn put_batch(
        &self,
        session_id: &str,
        after: u64,
        to: u64,
        body: Vec<u8>,
        archive: bool,
        context: &Context,
    ) -> std::result::Result<PutBatchResult, RadiusSyncError> {
        let encoded_id = encode_segment(session_id);
        let query = if archive {
            format!("to={to}&archive=1")
        } else {
            format!("to={to}")
        };
        let path = format!("/{encoded_id}/batches/{after}?{query}");
        let resp = self
            .send_request(
                reqwest::Method::PUT,
                &path,
                Some("application/gzip"),
                Some(body),
                context,
            )
            .await?;
        let status = resp.status().as_u16();
        if status == 201 {
            return Ok(PutBatchResult::Created);
        }
        if status == 409 {
            let body = resp.json::<Value>().await.ok();
            let err = error_of(body.as_ref());
            let code = err.code.as_deref();
            let details = &err.details;
            if code == Some("slot_taken")
                && let Some(slot_to) = details.get("to").and_then(is_seq)
                && let Some(device) = details.get("device").and_then(Value::as_str)
            {
                return Ok(PutBatchResult::Exists {
                    to: slot_to,
                    device: device.to_owned(),
                });
            }
            if code == Some("not_head")
                && let Some(head) = details.get("head").and_then(is_seq)
            {
                return Ok(PutBatchResult::NotHead { head });
            }
            if code == Some("slot_content_mismatch")
                && let Some(slot_to) = details.get("to").and_then(is_seq)
            {
                return Ok(PutBatchResult::NotHead { head: slot_to });
            }
            if matches!(code, Some("next_id_regression" | "legacy_session_history"))
                && let Some(head) = details.get("head").and_then(is_seq)
            {
                return Ok(PutBatchResult::NotHead { head });
            }
            let message = err.message.as_deref().unwrap_or("");
            let suffix = if message.is_empty() {
                String::new()
            } else {
                format!(" {message}")
            };
            return Err(RadiusSyncError::new(
                format!("Radius sync could not upload a batch: 409{suffix}"),
                409,
                err.code,
            ));
        }
        Err(response_failure(resp, "upload a batch").await)
    }

    pub async fn get_batch(
        &self,
        session_id: &str,
        after: u64,
        context: &Context,
    ) -> std::result::Result<Option<Vec<u8>>, RadiusSyncError> {
        let encoded_id = encode_segment(session_id);
        self.fetch_body(
            &format!("/{encoded_id}/batches/{after}"),
            context,
            "download a batch",
        )
        .await
    }

    pub async fn get_snapshot(
        &self,
        session_id: &str,
        to: u64,
        context: &Context,
    ) -> std::result::Result<Option<Vec<u8>>, RadiusSyncError> {
        let encoded_id = encode_segment(session_id);
        self.fetch_body(
            &format!("/{encoded_id}/snapshots/{to}"),
            context,
            "download a snapshot",
        )
        .await
    }

    pub async fn list(
        &self,
        session_id: &str,
        context: &Context,
    ) -> std::result::Result<RemoteListing, RadiusSyncError> {
        let encoded_id = encode_segment(session_id);
        let resp = self
            .send_request(
                reqwest::Method::GET,
                &format!("/{encoded_id}"),
                None,
                None,
                context,
            )
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(RemoteListing::default());
        }
        if !resp.status().is_success() {
            return Err(response_failure(resp, "list a session").await);
        }
        let body: Value = resp.json().await.map_err(|e| {
            RadiusSyncError::new(format!("Radius sync invalid listing: {e}"), 500, None)
        })?;
        listing_of(&body)
    }

    pub async fn sessions(
        &self,
        cursor: Option<&str>,
        context: &Context,
    ) -> std::result::Result<SessionChanges, RadiusSyncError> {
        let cursor = cursor.unwrap_or("0");
        let path = format!("?cursor={}&limit={PAGE_SIZE}", encode_segment(cursor));
        let resp = self
            .send_request(reqwest::Method::GET, &path, None, None, context)
            .await?;
        if !resp.status().is_success() {
            return Err(response_failure(resp, "list sessions").await);
        }
        let body: Value = resp.json().await.map_err(|e| {
            RadiusSyncError::new(format!("Radius sync invalid sessions page: {e}"), 500, None)
        })?;
        changes_of(&body)
    }

    pub async fn put_meta(
        &self,
        session_id: &str,
        meta: &Map<String, Value>,
        context: &Context,
    ) -> std::result::Result<(), RadiusSyncError> {
        let encoded_id = encode_segment(session_id);
        let body = serde_json::to_vec(meta)
            .map_err(|e| RadiusSyncError::new(format!("Invalid meta JSON: {e}"), 400, None))?;
        let resp = self
            .send_request(
                reqwest::Method::PUT,
                &format!("/{encoded_id}/meta"),
                Some("application/json"),
                Some(body),
                context,
            )
            .await?;
        if !resp.status().is_success() {
            return Err(response_failure(resp, "write a session description").await);
        }
        Ok(())
    }

    pub async fn get_meta(
        &self,
        session_id: &str,
        context: &Context,
    ) -> std::result::Result<Option<Map<String, Value>>, RadiusSyncError> {
        let encoded_id = encode_segment(session_id);
        let resp = self
            .send_request(
                reqwest::Method::GET,
                &format!("/{encoded_id}/meta"),
                None,
                None,
                context,
            )
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(response_failure(resp, "read a session description").await);
        }
        let meta: Value = resp.json().await.map_err(|e| {
            RadiusSyncError::new(format!("Radius sync invalid meta: {e}"), 500, None)
        })?;
        let Value::Object(map) = meta else {
            return Err(malformed("read a session description"));
        };
        Ok(Some(map))
    }

    async fn fetch_body(
        &self,
        path: &str,
        context: &Context,
        operation: &str,
    ) -> std::result::Result<Option<Vec<u8>>, RadiusSyncError> {
        let resp = self
            .send_request(reqwest::Method::GET, path, None, None, context)
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(response_failure(resp, operation).await);
        }
        let bytes = resp.bytes().await.map_err(|e| {
            RadiusSyncError::new(format!("Radius sync could not {operation}: {e}"), 500, None)
        })?;
        Ok(Some(bytes.to_vec()))
    }
}

fn map_sync_error(error: RadiusSyncError) -> pi_rs_chord::Error {
    if is_terminal_sync_error(&error) {
        pi_rs_chord::data::ChordError::new("backup_terminal", error.to_string()).into()
    } else {
        pi_rs_chord::Error::Invalid(error.to_string())
    }
}

impl pi_rs_chord::sync::AsyncRemote for RadiusSyncRemote {
    fn list(&self, id: String) -> SyncFuture<pi_rs_chord::sync::RemoteListing> {
        let remote = self.clone();
        Box::pin(async move {
            let ctx = Context::background();
            let listing = RadiusSyncRemote::list(&remote, &id, &ctx)
                .await
                .map_err(map_sync_error)?;
            let value = serde_json::to_value(listing)
                .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))?;
            serde_json::from_value(value).map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))
        })
    }

    fn get_batch(&self, id: String, after: u64) -> SyncFuture<Option<pi_rs_chord::sync::Body>> {
        let remote = self.clone();
        Box::pin(async move {
            let ctx = Context::background();
            let bytes = RadiusSyncRemote::get_batch(&remote, &id, after, &ctx)
                .await
                .map_err(map_sync_error)?;
            Ok(bytes.map(pi_rs_chord::sync::Body::from_bytes))
        })
    }

    fn get_snapshot(&self, id: String, to: u64) -> SyncFuture<Option<pi_rs_chord::sync::Body>> {
        let remote = self.clone();
        Box::pin(async move {
            let ctx = Context::background();
            let bytes = RadiusSyncRemote::get_snapshot(&remote, &id, to, &ctx)
                .await
                .map_err(map_sync_error)?;
            Ok(bytes.map(pi_rs_chord::sync::Body::from_bytes))
        })
    }

    fn put_batch(
        &self,
        id: String,
        batch: pi_rs_chord::sync::BatchInfo,
        body: pi_rs_chord::sync::Body,
    ) -> SyncFuture<pi_rs_chord::sync::PutBatchResult> {
        let remote = self.clone();
        Box::pin(async move {
            let ctx = Context::background();
            let bytes = body.collect().await?;
            let result = RadiusSyncRemote::put_batch(
                &remote,
                &id,
                batch.after,
                batch.to,
                bytes,
                batch.archive.unwrap_or(false),
                &ctx,
            )
            .await
            .map_err(map_sync_error)?;
            Ok(match result {
                PutBatchResult::Created => pi_rs_chord::sync::PutBatchResult::Created,
                PutBatchResult::Exists { to, device } => {
                    pi_rs_chord::sync::PutBatchResult::Exists { to, device }
                }
                PutBatchResult::NotHead { head } => {
                    pi_rs_chord::sync::PutBatchResult::NotHead { head }
                }
            })
        })
    }
}

async fn response_failure(resp: reqwest::Response, operation: &str) -> RadiusSyncError {
    let status = resp.status().as_u16();
    let body = resp.json::<Value>().await.ok();
    let detail = error_of(body.as_ref());
    let message = detail.message.as_deref().unwrap_or("");
    let suffix = if message.is_empty() {
        String::new()
    } else {
        format!(" {message}")
    };
    RadiusSyncError::new(
        format!("Radius sync could not {operation}: {status}{suffix}"),
        status,
        detail.code,
    )
}

struct ParsedError {
    message: Option<String>,
    code: Option<String>,
    details: Map<String, Value>,
}

fn is_seq(value: &Value) -> Option<u64> {
    value.as_u64()
}

fn malformed(operation: &str) -> RadiusSyncError {
    RadiusSyncError::new(
        format!("Radius sync could not {operation}: a malformed answer"),
        500,
        None,
    )
}

fn error_of(body: Option<&Value>) -> ParsedError {
    let Some(err) = body.and_then(|b| b.get("error")).and_then(Value::as_object) else {
        return ParsedError {
            message: None,
            code: None,
            details: Map::new(),
        };
    };
    ParsedError {
        message: err
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned),
        code: err.get("code").and_then(Value::as_str).map(str::to_owned),
        details: err
            .get("details")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
    }
}

pub fn listing_of(body: &Value) -> std::result::Result<RemoteListing, RadiusSyncError> {
    let Some(obj) = body.as_object() else {
        return Err(malformed("list a session"));
    };
    let (Some(raw_batches), Some(raw_snapshots)) = (
        obj.get("batches").and_then(Value::as_array),
        obj.get("snapshots").and_then(Value::as_array),
    ) else {
        return Err(malformed("list a session"));
    };
    let mut batches = Vec::with_capacity(raw_batches.len());
    for raw in raw_batches {
        let Some(slot) = raw.as_object() else {
            return Err(malformed("list a session"));
        };
        let (Some(after), Some(to), Some(device)) = (
            slot.get("after").and_then(is_seq),
            slot.get("to").and_then(is_seq),
            slot.get("device").and_then(Value::as_str),
        ) else {
            return Err(malformed("list a session"));
        };
        let mut normalized = Map::new();
        normalized.insert("after".into(), Value::from(after));
        normalized.insert("to".into(), Value::from(to));
        normalized.insert("device".into(), Value::from(device));
        if slot.get("folded") == Some(&Value::Bool(true)) {
            normalized.insert("folded".into(), Value::Bool(true));
        }
        if slot.get("archive") == Some(&Value::Bool(true)) {
            normalized.insert("archive".into(), Value::Bool(true));
        }
        batches.push(Value::Object(normalized));
    }
    let mut snapshots = Vec::with_capacity(raw_snapshots.len());
    for raw in raw_snapshots {
        let Some(at) = is_seq(raw) else {
            return Err(malformed("list a session"));
        };
        snapshots.push(Value::from(at));
    }
    Ok(RemoteListing { batches, snapshots })
}

pub fn changes_of(body: &Value) -> std::result::Result<SessionChanges, RadiusSyncError> {
    let Some(obj) = body.as_object() else {
        return Err(malformed("list sessions"));
    };
    let (Some(raw_sessions), Some(cursor), Some(more)) = (
        obj.get("sessions").and_then(Value::as_array),
        obj.get("cursor").and_then(is_seq),
        obj.get("more").and_then(Value::as_bool),
    ) else {
        return Err(malformed("list sessions"));
    };
    let mut sessions = Vec::with_capacity(raw_sessions.len());
    for raw in raw_sessions {
        let Some(session) = raw.as_object() else {
            return Err(malformed("list sessions"));
        };
        let (Some(id), Some(head)) = (
            session.get("id").and_then(Value::as_str),
            session.get("head").and_then(is_seq),
        ) else {
            return Err(malformed("list sessions"));
        };
        let meta = match session.get("meta") {
            Some(Value::Object(map)) => Some(map.clone()),
            Some(Value::Null) | None => None,
            Some(_) => return Err(malformed("list sessions")),
        };
        sessions.push(SessionChange {
            id: id.to_owned(),
            head,
            meta,
        });
    }
    Ok(SessionChanges {
        sessions,
        cursor: cursor.to_string(),
        more,
    })
}

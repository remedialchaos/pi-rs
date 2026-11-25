//! Pure-Rust attachment upload chunking, wire encoding, and submission payload assembly.
//!
//! Matches `p2.session-files` (`begin`, `append`, `finish`, `cancel`), `p2.attachments`
//! (`resolve`), and `p2.chat` (`send`) contracts from `crates/pi-rs-p2`.

use crate::browser_plugins::{
    ComposerAttachments, encode_call_frame, encode_call_frame_with_segments, indexes,
};
use crate::state::composer_state::{MAX_FILE_BYTES, PickedFile, attachment_keys};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Maximum upload size per file (25 MiB), matching `p2.session-files`.
pub const MAX_UPLOAD_BYTES: u64 = MAX_FILE_BYTES;
/// Maximum attachments allowed per chat submission, matching `p2.chat` schema.
pub const MAX_ATTACHMENTS_PER_MESSAGE: usize = 20;
/// Chunk size per `p2.session-files.append` call (512 KiB, below 1 MiB server `max_payload`).
pub const UPLOAD_CHUNK_BYTES: usize = 512 * 1024;

/// Returns the inline image MIME type for a filename, or `None` for non-images.
pub fn image_mime_type(name: &str) -> Option<&'static str> {
    let ext = name.rsplit('.').next()?;
    if ext.eq_ignore_ascii_case("png") {
        Some("image/png")
    } else if ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg") {
        Some("image/jpeg")
    } else if ext.eq_ignore_ascii_case("gif") {
        Some("image/gif")
    } else if ext.eq_ignore_ascii_case("webp") {
        Some("image/webp")
    } else {
        None
    }
}

/// Whether `name` is an inline-previewable image filename.
pub fn is_image_name(name: &str) -> bool {
    image_mime_type(name).is_some()
}

/// One sequential chunk of an upload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadChunk {
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub is_last: bool,
}

/// Splits `bytes` into sequential upload chunks of at most `chunk_size` bytes.
pub fn split_upload_chunks(bytes: &[u8], chunk_size: usize) -> Vec<UploadChunk> {
    let size = chunk_size.max(1);
    if bytes.is_empty() {
        return Vec::new();
    }
    let total = bytes.len();
    let mut chunks = Vec::new();
    let mut offset = 0usize;
    while offset < total {
        let end = (offset + size).min(total);
        chunks.push(UploadChunk {
            offset: offset as u64,
            bytes: bytes[offset..end].to_vec(),
            is_last: end == total,
        });
        offset = end;
    }
    chunks
}

/// Lifecycle phase of a single composer attachment upload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UploadPhase {
    Queued,
    Beginning,
    Uploading {
        sent: u64,
        total: u64,
    },
    Finishing,
    Uploaded {
        id: String,
        mime_type: Option<String>,
        size: u64,
    },
    Failed(String),
}

impl UploadPhase {
    /// Progress percentage in `0..=100`.
    pub fn progress_percent(&self) -> u8 {
        match self {
            Self::Queued | Self::Beginning => 0,
            Self::Uploading { sent, total } => (*sent)
                .saturating_mul(100)
                .checked_div(*total)
                .map_or(100, |p| p.min(99) as u8),
            Self::Finishing => 99,
            Self::Uploaded { .. } => 100,
            Self::Failed(_) => 0,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Beginning | Self::Uploading { .. } | Self::Finishing
        )
    }

    pub fn uploaded_id(&self) -> Option<&str> {
        match self {
            Self::Uploaded { id, .. } => Some(id.as_str()),
            _ => None,
        }
    }

    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::Failed(reason) => Some(reason.as_str()),
            _ => None,
        }
    }
}

/// Metadata returned by `p2.session-files.finish`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadedAttachment {
    pub id: String,
    pub name: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// Metadata returned by `p2.attachments.resolve`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedAttachment {
    pub id: String,
    pub name: String,
    pub path: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
}

/// Tracked upload entry for a composer file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadEntry {
    pub key: String,
    pub name: String,
    pub size: u64,
    pub preview: bool,
    pub mime_type: Option<String>,
    pub session: Option<String>,
    pub upload_token: Option<String>,
    pub phase: UploadPhase,
}

/// Result of enqueueing selected/dropped/pasted files into `ComposerAttachments`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnqueueResult {
    pub accepted_keys: Vec<String>,
    pub too_large: Option<String>,
    pub over_limit: Option<String>,
}

/// Error returned when assembling a `p2.chat.send` submission payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachmentSubmitError {
    UploadInProgress(String),
    UploadFailed { name: String, reason: String },
    TooManyAttachments { max: usize, count: usize },
    EmptyMessage,
}

impl std::fmt::Display for AttachmentSubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UploadInProgress(name) => {
                write!(f, "Wait for attachment upload to finish: {name}")
            }
            Self::UploadFailed { name, reason } => {
                write!(f, "Attachment upload failed ({name}): {reason}")
            }
            Self::TooManyAttachments { max, count } => {
                write!(f, "Too many attachments ({count}; maximum is {max})")
            }
            Self::EmptyMessage => write!(f, "Message cannot be empty"),
        }
    }
}

/// Ready-to-send `p2.chat.send` payload with resolved server attachment IDs.
#[derive(Clone, Debug, PartialEq)]
pub struct SendSubmissionPayload {
    pub text: String,
    pub attachments: Vec<String>,
    pub metadata: Vec<UploadedAttachment>,
    pub wire_args: Value,
}

/// Encodes `p2.session-files.begin` Call frame.
pub fn encode_begin_upload_call(call_id: u64, hold_id: u64, name: &str, source: &str) -> Vec<u8> {
    encode_call_frame(
        call_id,
        hold_id,
        indexes::session_files::METHOD_BEGIN,
        &json!({
            "space": "uploads",
            "name": name,
            "source": source,
        }),
    )
}

/// Encodes `p2.session-files.append` Call frame with a `$bytes: 0` binary segment.
pub fn encode_append_upload_call(
    call_id: u64,
    hold_id: u64,
    upload: &str,
    offset: u64,
    chunk: &[u8],
) -> Vec<u8> {
    encode_call_frame_with_segments(
        call_id,
        hold_id,
        indexes::session_files::METHOD_APPEND,
        &json!({
            "upload": upload,
            "offset": offset,
            "bytes": { "$bytes": 0 },
        }),
        &[chunk],
    )
}

/// Encodes `p2.session-files.finish` Call frame.
pub fn encode_finish_upload_call(call_id: u64, hold_id: u64, upload: &str) -> Vec<u8> {
    encode_call_frame(
        call_id,
        hold_id,
        indexes::session_files::METHOD_FINISH,
        &Value::String(upload.to_owned()),
    )
}

/// Encodes `p2.session-files.cancel` Call frame.
pub fn encode_cancel_upload_call(call_id: u64, hold_id: u64, upload: &str) -> Vec<u8> {
    encode_call_frame(
        call_id,
        hold_id,
        indexes::session_files::METHOD_CANCEL,
        &Value::String(upload.to_owned()),
    )
}

/// Encodes `p2.attachments.resolve` Call frame.
pub fn encode_resolve_attachments_call(
    call_id: u64,
    hold_id: u64,
    ids: &[String],
    conversation: u64,
) -> Vec<u8> {
    encode_call_frame(
        call_id,
        hold_id,
        indexes::attachments::METHOD_RESOLVE,
        &json!({
            "ids": ids,
            "conversation": conversation,
        }),
    )
}

/// Decodes `p2.session-files.begin` result (`{ id, upload }`).
pub fn decode_begin_upload_result(value: &Value) -> Result<(String, String), String> {
    let id = value["id"]
        .as_str()
        .ok_or_else(|| "missing upload id".to_owned())?;
    let upload = value["upload"]
        .as_str()
        .ok_or_else(|| "missing upload token".to_owned())?;
    Ok((id.to_owned(), upload.to_owned()))
}

/// Decodes `p2.session-files.finish` result (`{ id, name, size, mimeType? }`).
pub fn decode_finish_upload_result(value: &Value) -> Result<UploadedAttachment, String> {
    let id = value["id"]
        .as_str()
        .ok_or_else(|| "missing finished attachment id".to_owned())?
        .to_owned();
    let name = value["name"]
        .as_str()
        .ok_or_else(|| "missing finished attachment name".to_owned())?
        .to_owned();
    let size = value["size"]
        .as_u64()
        .ok_or_else(|| "missing finished attachment size".to_owned())?;
    let mime_type = value["mimeType"].as_str().map(str::to_owned);
    Ok(UploadedAttachment {
        id,
        name,
        size,
        mime_type,
    })
}

/// Decodes `p2.attachments.resolve` result array.
pub fn decode_resolved_attachments(value: &Value) -> Vec<ResolvedAttachment> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| serde_json::from_value::<ResolvedAttachment>(item.clone()).ok())
        .collect()
}

/// Rewrites `(attachment:<old_key>)` links in `draft` to `(attachment:<new_id>)`.
pub fn rewrite_attachment_reference(draft: &str, old_key: &str, new_id: &str) -> String {
    if old_key.is_empty() || old_key == new_id {
        return draft.to_owned();
    }
    draft.replace(
        &format!("](attachment:{old_key})"),
        &format!("](attachment:{new_id})"),
    )
}

impl ComposerAttachments {
    /// Enqueues files into both `files` and `uploads`, enforcing size and per-message caps.
    pub fn enqueue_files<F>(
        &mut self,
        session: Option<&str>,
        chosen: &[(String, u64)],
        key_for: F,
    ) -> EnqueueResult
    where
        F: Fn(usize) -> String,
    {
        let mut result = EnqueueResult::default();
        for (idx, (name, size)) in chosen.iter().enumerate() {
            if *size > MAX_UPLOAD_BYTES {
                if result.too_large.is_none() {
                    result.too_large = Some(name.clone());
                }
                continue;
            }
            if self.files.len() >= MAX_ATTACHMENTS_PER_MESSAGE {
                if result.over_limit.is_none() {
                    result.over_limit = Some(name.clone());
                }
                continue;
            }
            let key = key_for(idx);
            let mime_type = image_mime_type(name).map(str::to_owned);
            let preview = mime_type.is_some();
            self.files.push(PickedFile {
                key: key.clone(),
                name: name.clone(),
                preview,
                uploaded: None,
                session: session.map(str::to_owned),
            });
            self.uploads.insert(
                key.clone(),
                UploadEntry {
                    key: key.clone(),
                    name: name.clone(),
                    size: *size,
                    preview,
                    mime_type,
                    session: session.map(str::to_owned),
                    upload_token: None,
                    phase: UploadPhase::Queued,
                },
            );
            result.accepted_keys.push(key);
        }
        self.last_too_large = result.too_large.clone();
        self.last_error = result
            .too_large
            .as_ref()
            .map(|name| format!("{name} exceeds 25 MiB"))
            .or_else(|| {
                result
                    .over_limit
                    .as_ref()
                    .map(|name| format!("Cannot attach {name}: maximum 20 attachments"))
            });
        result
    }

    pub fn mark_beginning(&mut self, key: &str) {
        if let Some(entry) = self.uploads.get_mut(key) {
            entry.phase = UploadPhase::Beginning;
        }
    }

    pub fn mark_uploading(&mut self, key: &str, upload_token: String, sent: u64, total: u64) {
        if let Some(entry) = self.uploads.get_mut(key) {
            entry.upload_token = Some(upload_token);
            entry.phase = UploadPhase::Uploading { sent, total };
        }
    }

    pub fn mark_finishing(&mut self, key: &str) {
        if let Some(entry) = self.uploads.get_mut(key) {
            entry.phase = UploadPhase::Finishing;
        }
    }

    pub fn mark_uploaded(&mut self, key: &str, uploaded: UploadedAttachment) {
        if let Some(file) = self.files.iter_mut().find(|f| f.key == key) {
            file.uploaded = Some(uploaded.id.clone());
        }
        if let Some(entry) = self.uploads.get_mut(key) {
            entry.mime_type = uploaded.mime_type.clone().or(entry.mime_type.clone());
            entry.phase = UploadPhase::Uploaded {
                id: uploaded.id,
                mime_type: uploaded.mime_type,
                size: uploaded.size,
            };
        }
        self.last_error = None;
    }

    pub fn mark_failed(&mut self, key: &str, reason: String) {
        if let Some(entry) = self.uploads.get_mut(key) {
            entry.phase = UploadPhase::Failed(reason.clone());
        }
        self.last_error = Some(reason);
    }

    pub fn has_in_flight(&self) -> bool {
        self.uploads.values().any(|entry| entry.phase.is_active())
    }

    pub fn has_ready_attachments(&self) -> bool {
        self.files.iter().any(|file| {
            file.uploaded.is_some()
                || self
                    .uploads
                    .get(&file.key)
                    .and_then(|entry| entry.phase.uploaded_id())
                    .is_some()
        })
    }

    pub fn upload_token_for(&self, key: &str) -> Option<&str> {
        self.uploads
            .get(key)
            .and_then(|entry| entry.upload_token.as_deref())
    }

    /// Validates all composer attachments and assembles the `p2.chat.send` payload.
    ///
    /// Referenced attachments come first in draft order, followed by any other uploaded
    /// attachments in composer order (deduplicated). Keys referenced in `draft` are
    /// rewritten to their server-assigned upload IDs so `p2.chat` can match them.
    pub fn assemble_send_payload(
        &self,
        draft: &str,
        request_id: &str,
        when_busy: &str,
    ) -> Result<SendSubmissionPayload, AttachmentSubmitError> {
        let mut resolved_ids = Vec::new();
        let mut metadata = Vec::new();
        let mut rewritten_draft = draft.to_owned();

        let mut push_file = |file: &PickedFile| -> Result<(), AttachmentSubmitError> {
            let entry = self.uploads.get(&file.key);
            if let Some(entry) = entry {
                if entry.phase.is_active() {
                    return Err(AttachmentSubmitError::UploadInProgress(entry.name.clone()));
                }
                if let Some(reason) = entry.phase.error_message() {
                    return Err(AttachmentSubmitError::UploadFailed {
                        name: entry.name.clone(),
                        reason: reason.to_owned(),
                    });
                }
            }
            let server_id = file
                .uploaded
                .as_deref()
                .or_else(|| entry.and_then(|e| e.phase.uploaded_id()))
                .ok_or_else(|| AttachmentSubmitError::UploadInProgress(file.name.clone()))?;
            rewritten_draft = rewrite_attachment_reference(&rewritten_draft, &file.key, server_id);
            if !resolved_ids.iter().any(|existing| existing == server_id) {
                resolved_ids.push(server_id.to_owned());
                let (size, mime_type) = match entry {
                    Some(UploadEntry {
                        phase:
                            UploadPhase::Uploaded {
                                mime_type, size, ..
                            },
                        ..
                    }) => (*size, mime_type.clone()),
                    Some(e) => (e.size, e.mime_type.clone()),
                    None => (0, None),
                };
                metadata.push(UploadedAttachment {
                    id: server_id.to_owned(),
                    name: file.name.clone(),
                    size,
                    mime_type,
                });
            }
            Ok(())
        };

        for key in attachment_keys(draft) {
            if let Some(file) = self.files.iter().find(|f| f.key == key) {
                push_file(file)?;
            }
        }
        for file in &self.files {
            push_file(file)?;
        }

        if resolved_ids.len() > MAX_ATTACHMENTS_PER_MESSAGE {
            return Err(AttachmentSubmitError::TooManyAttachments {
                max: MAX_ATTACHMENTS_PER_MESSAGE,
                count: resolved_ids.len(),
            });
        }
        if resolved_ids.is_empty() && rewritten_draft.trim().is_empty() {
            return Err(AttachmentSubmitError::EmptyMessage);
        }

        let mut wire_args = json!({
            "text": rewritten_draft,
            "requestId": request_id,
            "whenBusy": when_busy,
        });
        if !resolved_ids.is_empty() {
            wire_args["attachments"] = json!(resolved_ids);
        }

        Ok(SendSubmissionPayload {
            text: rewritten_draft,
            attachments: resolved_ids,
            metadata,
            wire_args,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chord_bridge::{decode_part, decode_varint};

    #[test]
    fn split_upload_chunks_handles_empty_exact_and_partial() {
        assert!(split_upload_chunks(&[], 4).is_empty());

        let exact = split_upload_chunks(b"abcdefgh", 4);
        assert_eq!(exact.len(), 2);
        assert_eq!(exact[0].offset, 0);
        assert_eq!(exact[0].bytes, b"abcd");
        assert!(!exact[0].is_last);
        assert_eq!(exact[1].offset, 4);
        assert_eq!(exact[1].bytes, b"efgh");
        assert!(exact[1].is_last);

        let partial = split_upload_chunks(b"abcde", 2);
        assert_eq!(partial.len(), 3);
        assert_eq!(partial[0].offset, 0);
        assert_eq!(partial[1].offset, 2);
        assert_eq!(partial[2].offset, 4);
        assert_eq!(partial[2].bytes, b"e");
        assert!(partial[2].is_last);
    }

    #[test]
    fn append_call_frame_encodes_binary_segment_with_dollar_bytes() {
        let frame = encode_append_upload_call(11, 5, "tok-1", 1024, b"hello-chunk");
        assert_eq!(frame[0], 3);
        let mut pos = 1;
        let (call_id, n) = decode_varint(&frame[pos..]).expect("call_id");
        pos += n;
        let (hold_id, n) = decode_varint(&frame[pos..]).expect("hold_id");
        pos += n;
        let (method, n) = decode_varint(&frame[pos..]).expect("method");
        pos += n;
        let (flags, n) = decode_varint(&frame[pos..]).expect("flags");
        pos += n;
        assert_eq!(call_id, 11);
        assert_eq!(hold_id, 5);
        assert_eq!(method, indexes::session_files::METHOD_APPEND);
        assert_eq!(flags, 3);

        let (part_bytes, deflated) = decode_part(&frame[pos..]).expect("part");
        assert!(!deflated);
        let json_body: Value = serde_json::from_slice(&part_bytes).expect("json");
        assert_eq!(
            json_body,
            json!({
                "upload": "tok-1",
                "offset": 1024,
                "bytes": { "$bytes": 0 }
            })
        );
        let (_, part_hdr_len) = decode_varint(&frame[pos..]).expect("part_hdr");
        pos += part_hdr_len + part_bytes.len();

        let (seg_count, n) = decode_varint(&frame[pos..]).expect("seg_count");
        pos += n;
        assert_eq!(seg_count, 1);
        let (seg_len, n) = decode_varint(&frame[pos..]).expect("seg_len");
        pos += n;
        assert_eq!(seg_len, 11);
        assert_eq!(&frame[pos..], b"hello-chunk");
    }

    #[test]
    fn begin_finish_cancel_and_resolve_wire_calls_match_contract() {
        let begin = encode_begin_upload_call(1, 2, "pic.png", "web");
        assert_eq!(begin[0], 3);
        let begin_res =
            decode_begin_upload_result(&json!({ "id": "id-1", "upload": "up-1" })).expect("begin");
        assert_eq!(begin_res, ("id-1".into(), "up-1".into()));

        let finish = encode_finish_upload_call(2, 2, "up-1");
        assert_eq!(finish[0], 3);
        let finished = decode_finish_upload_result(&json!({
            "id": "id-1",
            "name": "pic.png",
            "size": 42,
            "mimeType": "image/png"
        }))
        .expect("finish");
        assert_eq!(finished.mime_type.as_deref(), Some("image/png"));

        let cancel = encode_cancel_upload_call(3, 2, "up-1");
        assert_eq!(cancel[0], 3);

        let resolve = encode_resolve_attachments_call(4, 9, &["id-1".into()], 2);
        assert_eq!(resolve[0], 3);
        let resolved = decode_resolved_attachments(&json!([{
            "id": "id-1",
            "name": "pic.png",
            "path": "/tmp/id-1-pic.png",
            "size": 42,
            "mimeType": "image/png"
        }]));
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].id, "id-1");
    }

    #[test]
    fn attachment_lifecycle_and_submission_payload_assembly() {
        let mut attachments = ComposerAttachments::default();
        let enq = attachments.enqueue_files(
            Some("sess-1"),
            &[
                ("pic.png".into(), 1024),
                ("huge.bin".into(), MAX_UPLOAD_BYTES + 1),
                ("notes.txt".into(), 50),
            ],
            |i| format!("local-{i}"),
        );
        assert_eq!(enq.accepted_keys, vec!["local-0", "local-2"]);
        assert_eq!(enq.too_large.as_deref(), Some("huge.bin"));
        assert!(attachments.has_in_flight());

        // Cannot submit while uploads are still in flight.
        assert!(matches!(
            attachments.assemble_send_payload(
                "see ![pic.png](attachment:local-0)",
                "req-1",
                "steer"
            ),
            Err(AttachmentSubmitError::UploadInProgress(_))
        ));

        attachments.mark_beginning("local-0");
        attachments.mark_uploading("local-0", "up-0".into(), 512, 1024);
        assert_eq!(attachments.upload_token_for("local-0"), Some("up-0"));
        assert_eq!(attachments.uploads["local-0"].phase.progress_percent(), 50);
        attachments.mark_finishing("local-0");
        attachments.mark_uploaded(
            "local-0",
            UploadedAttachment {
                id: "srv-pic".into(),
                name: "pic.png".into(),
                size: 1024,
                mime_type: Some("image/png".into()),
            },
        );
        attachments.mark_failed("local-2", "network lost".into());
        assert!(matches!(
            attachments.assemble_send_payload("hi", "req-1", "steer"),
            Err(AttachmentSubmitError::UploadFailed { .. })
        ));

        // Removing the failed attachment allows submission with the uploaded one.
        attachments.remove("local-2");
        assert!(!attachments.has_in_flight());
        assert!(attachments.has_ready_attachments());

        let payload = attachments
            .assemble_send_payload("check ![pic.png](attachment:local-0)", "req-2", "steer")
            .expect("ready payload");
        assert_eq!(payload.text, "check ![pic.png](attachment:srv-pic)");
        assert_eq!(payload.attachments, vec!["srv-pic"]);
        assert_eq!(payload.wire_args["attachments"], json!(["srv-pic"]));
        assert_eq!(payload.wire_args["requestId"], json!("req-2"));

        // Empty text is allowed when at least one uploaded attachment is present.
        let empty_text = attachments
            .assemble_send_payload("   ", "req-3", "steer")
            .expect("attachment-only send");
        assert_eq!(empty_text.attachments, vec!["srv-pic"]);

        // Empty text with no attachments is rejected.
        attachments.clear();
        assert_eq!(
            attachments.assemble_send_payload("   ", "req-4", "steer"),
            Err(AttachmentSubmitError::EmptyMessage)
        );
    }
}

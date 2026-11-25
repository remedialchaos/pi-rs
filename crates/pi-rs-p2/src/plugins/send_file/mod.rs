//! `send-file` built-in plugin: `send_file` tool for offering private download snapshots
//! (`p2.send-file-tools`) and UTF-8 safe preview (`p2.sent-files`).

use std::sync::Arc;

use pi_rs_ai::{InputContent, Tool};
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::Contract,
    data::ChordError,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
    sync::SyncFuture,
};
use pi_rs_durable::{
    harness::{Extension, ToolRegistration, define_extension},
    session::Error as DurableError,
    tools::ToolResult,
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    api::v1::{
        FacetKind, PluginRegistry, Principal, p2_contracts::lookup, server::service_of,
        worker::worker_of,
    },
    runtime::{
        files::DownloadInfo,
        session_files::{Files, find_download, save_download},
    },
};

/// Maximum bytes read into a `SentFiles.preview` response (matches upstream `MAX_SENT_PREVIEW`).
pub const MAX_SENT_PREVIEW: usize = 64 * 1024;

/// Host-lifetime native service key for the worker's `Files` handle.
pub const WORKER_FILES_SERVICE: &str = "p2.worker-files";

pub fn offered_by(conversation: u64) -> String {
    format!("conversation:{conversation}")
}

/// Decodes `bytes` as UTF-8 without NUL bytes; trims up to 3 trailing bytes of a split
/// code point when `partial` is true.
pub fn preview_text(bytes: &[u8], partial: bool) -> Option<String> {
    if bytes.contains(&0) {
        return None;
    }
    let max_cut = if partial { 3 } else { 0 };
    for cut in 0..=max_cut {
        if cut > bytes.len() {
            break;
        }
        if let Ok(text) = std::str::from_utf8(&bytes[..bytes.len() - cut]) {
            return Some(text.to_owned());
        }
    }
    None
}

pub type SaveSnapshotFn =
    Arc<dyn Fn(String, u64, u64, Vec<u8>, Context) -> SyncFuture<DownloadInfo> + Send + Sync>;

#[derive(Deserialize)]
struct SendFileArgs {
    path: String,
    name: Option<String>,
}

/// Builds the Durable `p2-downloads` extension containing the `send_file` tool.
pub fn create_send_file_extension(save: SaveSnapshotFn) -> Extension {
    let tool = ToolRegistration {
        declaration: Tool {
            name: "send_file".into(),
            description: "Offer a file for browser download. Path is relative to your cwd. Copies a private snapshot (maximum 25 MiB) and shows a Download link to all authorized viewers of this session. Downloads require a click; do not export secrets. Snapshots are retained.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "minLength": 1 },
                    "name": { "type": "string", "minLength": 1 }
                },
                "required": ["path"],
                "additionalProperties": true
            }),
            constrained_sampling: None,
        },
        sequential: false,
        prepare_arguments: None,
        cancel: None,
        structured_output_schema: None,
        output_limits: None,
        replay_safe: false,
        callers: None,
        deferred: false,
        execute: Arc::new(move |args, invocation, context| {
            let save = save.clone();
            Box::pin(async move {
                let parsed: SendFileArgs = serde_json::from_value(args)?;
                let env = invocation
                    .env(context)
                    .await?
                    .ok_or_else(|| DurableError::invalid("No execution environment"))?;
                let bytes = env
                    .read_binary_file(&parsed.path, context)
                    .await
                    .map_err(|_| DurableError::invalid("Could not open source file"))?;
                let file_name = parsed.name.unwrap_or_else(|| {
                    std::path::Path::new(&parsed.path)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("file")
                        .to_owned()
                });
                let size = bytes.len() as u64;
                let saved = save(
                    file_name,
                    size,
                    invocation.record.conversation_id,
                    bytes,
                    context.clone(),
                )
                .await
                .map_err(|e| DurableError::invalid(e.to_string()))?;
                Ok(ToolResult {
                    output: Some(vec![InputContent::Text(
                        format!("Download ready: {} ({} bytes).", saved.name, saved.size).into(),
                    )]),
                    details: Some(serde_json::to_value(&saved)?),
                    ..Default::default()
                }
                .into())
            })
        }),
    };

    define_extension(Extension {
        name: "p2-downloads".into(),
        tools: vec![tool],
        ..Default::default()
    })
}

#[derive(Deserialize)]
struct PreviewArgs {
    id: String,
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let tools_contract = Arc::new(Contract::local(
        "p2.send-file-tools",
        1,
        Vec::new(),
        indexmap::IndexMap::new(),
    )?);
    let sent_contract = Arc::new(lookup("p2.sent-files")?);

    registry.register(
        "send-file",
        FacetKind::Worker,
        define_facet(move |host| {
            host.serve(
                tools_contract.clone(),
                move |_, _, object| {
                    let host_handle = object.host();
                    if let Ok(worker) = worker_of(&host_handle) {
                        let files_host = host_handle.clone();
                        let save: SaveSnapshotFn =
                            Arc::new(move |name, size, conversation, bytes, _ctx| {
                                let files_host = files_host.clone();
                                Box::pin(async move {
                                    let files =
                                        service_of::<Files>(&files_host, WORKER_FILES_SERVICE)
                                            .map_err(|_| {
                                                Error::Invalid(
                                                    "Session files store unavailable".into(),
                                                )
                                            })?;
                                    let mut remaining = Some(bytes);
                                    save_download(
                                        files.as_ref(),
                                        &name,
                                        size,
                                        "agent",
                                        Some(offered_by(conversation)),
                                        move || {
                                            let chunk = remaining.take().unwrap_or_default();
                                            async move { Ok(chunk) }
                                        },
                                    )
                                    .await
                                    .map_err(|e| Error::Invalid(e.to_string()))
                                })
                            });
                        let extension = create_send_file_extension(save);
                        let ext_name = extension.name.clone();
                        let registries = worker.registries.clone();
                        registries.extensions.install(extension)?;
                        let _cleanup = object.open(
                            |_| Box::pin(async { Ok(()) }),
                            move |_, _| {
                                let registries = registries.clone();
                                let ext_name = ext_name.clone();
                                Box::pin(async move {
                                    let _ = registries.extensions.uninstall(&ext_name);
                                    Ok(())
                                })
                            },
                        );
                    }
                    Ok(Members::new())
                },
                ServeOptions::default(),
            )?;

            host.serve(
                sent_contract.clone(),
                move |params, principal, object| {
                    let session = params.to_serde()?["session"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("missing session param".into()))?
                        .to_owned();
                    let parsed = principal
                        .and_then(|v| Principal::deserialize(v).ok())
                        .ok_or_else(|| ChordError::new("forbidden", "viewers only"))?;
                    if !parsed.can_view(&session) {
                        return Err(ChordError::new("forbidden", "viewers only").into());
                    }
                    let host_handle = object.host();
                    let mut members = Members::variant("reader");
                    members.method("preview", move |args, _| {
                        let host_handle = host_handle.clone();
                        Box::pin(async move {
                            let parsed: PreviewArgs = serde_json::from_value(args.to_serde()?)?;
                            let files = service_of::<Files>(&host_handle, WORKER_FILES_SERVICE)
                                .map_err(|_| Error::Invalid("File not found".into()))?;
                            let (file, name) = find_download(files.as_ref(), &parsed.id)
                                .ok_or_else(|| Error::Invalid("File not found".into()))?;
                            let length = (file.size as usize).min(MAX_SENT_PREVIEW);
                            let bytes = if length == 0 {
                                Vec::new()
                            } else {
                                files.read(&file.id, 0, length)?
                            };
                            let truncated = file.size > bytes.len() as u64;
                            let text = preview_text(&bytes, truncated);
                            Ok(JsonValue::from_serde(json!({
                                "name": name,
                                "size": file.size,
                                "text": text,
                                "truncated": truncated,
                            })))
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )
}

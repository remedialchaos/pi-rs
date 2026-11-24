mod backup_store;
mod upload;
mod worker;

pub use backup_store::BackupStore;
pub use upload::{Begun, CHUNK_BYTES, Uploads};

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

use pi_rs_chord::{
    Error, Result,
    context::Context,
    data::ChordError,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
    state::Source,
    sync::{
        AsyncRemote, FileSystemSyncRemote, FlushScheduler, PutBatchResult, SyncFuture,
        check_session_id,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{
    api::v1::{FacetKind, PluginRegistry, Principal, p2_contracts::lookup},
    runtime::{
        host_dirs::server_directory,
        logs::{LogLevel, process_logs},
        server::{install_service, server_of},
    },
};

pub const BACKUP_TARGETS: &str = "p2.backup-targets";
pub const BACKUP_UPLOADS: &str = "p2.backup-uploads";
const BACKUP_TERMINAL: &str = "backup_terminal";

pub type TerminalError = Arc<dyn Fn(&Error) -> bool + Send + Sync>;

fn is_terminal(error: &Error) -> bool {
    matches!(error, Error::Chord(error) if error.code == BACKUP_TERMINAL)
}

#[derive(Clone)]
pub struct OpenBackupTarget {
    pub remote: Arc<dyn AsyncRemote>,
    pub device: String,
    pub server: String,
    pub terminal: Option<TerminalError>,
}

pub type OpenTarget = Arc<dyn Fn() -> SyncFuture<OpenBackupTarget> + Send + Sync>;

#[derive(Default)]
pub struct BackupTargets(RwLock<HashMap<String, OpenTarget>>);

impl BackupTargets {
    pub fn offer(&self, name: &str, target: OpenTarget) -> Result<()> {
        check_session_id(name)?;
        self.0
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .insert(name.into(), target);
        Ok(())
    }

    pub fn remove(&self, name: &str) {
        self.0
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .remove(name);
    }

    pub async fn open(&self, name: &str) -> Result<OpenBackupTarget> {
        let target = self
            .0
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(name)
            .cloned()
            .ok_or_else(|| {
                ChordError::new(
                    "unavailable",
                    format!("P2_BACKUP={name}: no loaded plugin offers this backup target"),
                )
            })?;
        target().await
    }
}

enum Target {
    Folder(PathBuf),
    Named(String),
}
impl Target {
    fn configured() -> Result<Option<Self>> {
        let folder = std::env::var_os("P2_BACKUP_DIR").filter(|value| !value.is_empty());
        let name = std::env::var("P2_BACKUP")
            .ok()
            .filter(|name| !name.is_empty());
        match (folder, name) {
            (Some(_), Some(_)) => Err(Error::Invalid(
                "Set P2_BACKUP_DIR (a folder) or P2_BACKUP (a target such as radius), not both"
                    .into(),
            )),
            (Some(path), None) => Ok(Some(Self::Folder(path.into()))),
            (None, Some(name)) => Ok(Some(Self::Named(name))),
            (None, None) => Ok(None),
        }
    }

    fn key(&self) -> String {
        match self {
            Self::Folder(path) => {
                let digest = format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes()));
                format!("folder-{}", &digest[..12])
            }
            Self::Named(name) => name.clone(),
        }
    }

    async fn open(&self, data_dir: &Path, targets: &BackupTargets) -> Result<OpenBackupTarget> {
        match self {
            Self::Folder(path) => {
                let directory = data_dir.to_owned();
                let device = tokio::task::spawn_blocking(move || -> Result<String> {
                    std::fs::create_dir_all(&directory)?;
                    let lock = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(directory.join("device.json.lock"))?;
                    lock.lock()?;
                    let path = directory.join("device.json");
                    let mut state = match std::fs::read(path) {
                        Ok(data) => serde_json::from_slice::<Value>(&data)?,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
                        Err(error) => return Err(error.into()),
                    };
                    if let Some(id) = state["id"].as_str().filter(|id| !id.is_empty()) {
                        return Ok(id.into());
                    }
                    if !state.is_object() {
                        state = json!({});
                    }
                    let id = Uuid::new_v4().to_string();
                    state["id"] = json!(&id);
                    backup_store::atomic_write(
                        &directory,
                        "device.json",
                        &serde_json::to_vec(&state)?,
                    )?;
                    Ok(id)
                })
                .await
                .map_err(|error| Error::Invalid(error.to_string()))??;
                Ok(OpenBackupTarget {
                    remote: Arc::new(FileSystemSyncRemote::new(data_dir.join(path))),
                    device,
                    server: "server".into(),
                    terminal: None,
                })
            }
            Self::Named(name) => targets.open(name).await,
        }
    }
}

#[derive(Clone)]
struct Reports {
    state: Source,
    values: Arc<Mutex<serde_json::Map<String, Value>>>,
}

impl Reports {
    fn report(&self, store: &str, status: Value) -> Result<()> {
        if let Some(state @ ("error" | "behind" | "diverged")) = status["state"].as_str() {
            let message = status["error"]
                .as_str()
                .map_or_else(|| state.to_owned(), |error| format!("{state}: {error}"));
            process_logs().append(&format!("backup:{store}"), LogLevel::Event, &message);
        }
        let mut values = self
            .values
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        values.insert(store.into(), status);
        self.state
            .set(Value::Object(values.clone()).into(), &Context::background())
    }
}

/// Native service for scheduled and manually triggered backups.
#[derive(Default)]
pub struct BackupService {
    live: Mutex<Option<Arc<LiveBackup>>>,
}

struct LiveBackup {
    uploads: Uploads,
    server: Arc<BackupStore>,
    scheduler: FlushScheduler,
    observer: JoinHandle<()>,
    reports: Reports,
}

impl BackupService {
    fn opened(&self) -> Result<Arc<LiveBackup>> {
        self.live
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or_else(|| Error::Invalid("Backup is off".into()))
    }

    /// Flush the server store immediately.
    pub async fn run(&self) -> Result<()> {
        let live = self.opened()?;
        live.server
            .run(|status| {
                let _ = live.reports.report("server", status);
            })
            .await
    }
}

#[derive(Deserialize)]
struct Slot {
    after: u64,
    to: u64,
    #[serde(default)]
    archive: bool,
}

#[derive(Deserialize)]
struct Chunk {
    upload: String,
    bytes: Vec<u8>,
}

#[derive(Deserialize)]
struct UploadId {
    upload: String,
}

fn put_result(result: PutBatchResult) -> Value {
    match result {
        PutBatchResult::Created => json!({"type": "created"}),
        PutBatchResult::Exists { to, device } => {
            json!({"type": "exists", "to": to, "device": device})
        }
        PutBatchResult::NotHead { head } => json!({"type": "not_head", "head": head}),
        PutBatchResult::StaleEpoch { epoch } => json!({"type": "stale_epoch", "epoch": epoch}),
    }
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let backup = Arc::new(lookup("p2.backup")?);
    let remote = Arc::new(lookup("p2.backup-remote")?);
    registry.register("backup", FacetKind::Worker, worker::facet(remote.clone())?)?;
    registry.register(
        "backup",
        FacetKind::Server,
        define_facet(move |facet| {
            let host = facet.host();
            let targets = Arc::new(BackupTargets::default());
            let service = Arc::new(BackupService::default());
            install_service(&host, BACKUP_TARGETS, &targets)?;
            install_service(&host, BACKUP_UPLOADS, &service)?;
            let statuses = Reports {
                state: Source::new(host.batch(), json!({}).into())?,
                values: Arc::default(),
            };
            let device = Source::new(host.batch(), JsonValue::Null)?;
            let local_service = service.clone();
            let local_statuses = statuses.clone();
            let local_device = device.clone();
            facet.serve(
                backup.clone(),
                move |_, _, object| {
                    let host = object.host();
                    let device = local_device.clone();
                    let statuses = local_statuses.clone();
                    let targets = targets.clone();
                    let service = local_service.clone();
                    let closing = service.clone();
                    let reports = statuses.clone();
                    let open_device = device.clone();
                    let close_device = device.clone();
                    object.open(
                        move |ctx| {
                            Box::pin(async move {
                                let Some(target) = Target::configured()? else {
                                    return Ok(());
                                };
                                let natives = server_of(&host)?;
                                let data = Path::new(&natives.info.data_dir);
                                let OpenBackupTarget {
                                    remote,
                                    device,
                                    server,
                                    terminal,
                                } = target.open(data, &targets).await.map_err(|error| {
                                    ChordError::new("unavailable", error.to_string())
                                })?;
                                let uploads = Uploads::new(remote.clone(), device.clone())
                                    .with_terminal(terminal.clone());
                                let mut changes = natives.store().changes();
                                let server = Arc::new(
                                    BackupStore::open(
                                        natives.store().clone(),
                                        server,
                                        &server_directory(data),
                                        &target.key(),
                                        remote,
                                        device.clone(),
                                    )
                                    .await?
                                    .with_terminal(terminal),
                                );
                                let scheduled = server.clone();
                                let scheduled_reports = reports.clone();
                                let scheduler = FlushScheduler::new(
                                    Default::default(),
                                    move || {
                                        let scheduled = scheduled.clone();
                                        let reports = scheduled_reports.clone();
                                        Box::pin(async move {
                                            scheduled
                                                .run(|status| {
                                                    let _ = reports.report("server", status);
                                                })
                                                .await
                                        })
                                    },
                                    |_| {},
                                );
                                scheduler.notify(false);
                                let notify = scheduler.clone();
                                let observer = tokio::spawn(async move {
                                    while changes.changed().await.is_ok() {
                                        notify.notify(false);
                                    }
                                });
                                let live = Arc::new(LiveBackup {
                                    uploads,
                                    server,
                                    scheduler,
                                    observer,
                                    reports,
                                });
                                *service
                                    .live
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner()) = Some(live);
                                open_device.set(json!(device).into(), &ctx.context)?;
                                Ok(())
                            })
                        },
                        move |_, _| {
                            Box::pin(async move {
                                let _ = close_device.set(JsonValue::Null, &Context::background());
                                let live = closing
                                    .live
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .take();
                                if let Some(live) = live {
                                    live.observer.abort();
                                    live.uploads.close().await;
                                    live.scheduler.close().await?;
                                }
                                Ok(())
                            })
                        },
                    );
                    let mut members = Members::new();
                    members.state("device", device)?;
                    members.state("statuses", statuses.state.clone())?;
                    members.method("report", move |args, _| {
                        let statuses = statuses.clone();
                        Box::pin(async move {
                            #[derive(Deserialize)]
                            struct Report {
                                store: String,
                                status: Value,
                            }
                            let report: Report = serde_json::from_value(args.to_serde()?)?;
                            statuses.report(&report.store, report.status)?;
                            Ok(JsonValue::Null)
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )?;

            let local = backup.clone();
            facet.serve(
                remote.clone(),
                move |_, principal, object| {
                    let Some(Principal::Worker { session, .. }) = principal
                        .cloned()
                        .map(serde_json::from_value::<Principal>)
                        .transpose()?
                    else {
                        return Err(ChordError::new(
                            "forbidden",
                            "the backup remote is for session workers",
                        )
                        .into());
                    };
                    let store = format!("session-{session}");
                    check_session_id(&store)?;
                    let local = object.get(local.clone(), json!({}).into())?;
                    let mut members = Members::variant("worker");
                    members.state("device", device.clone())?;
                    members.method("report", move |args, ctx| {
                        let local = local.clone();
                        let store = store.clone();
                        Box::pin(async move {
                            ctx.call(
                                &local,
                                "report",
                                json!({"store": store, "status": args.to_serde()?["status"]})
                                    .into(),
                            )
                            .await
                        })
                    })?;
                    let store = format!("session-{session}");
                    for name in ["begin", "append", "finish", "abort"] {
                        let service = service.clone();
                        let store = store.clone();
                        members.method(name, move |args, _| {
                            let service = service.clone();
                            let store = store.clone();
                            Box::pin(async move {
                                let live = service.opened()?;
                                let args = args.to_serde()?;
                                let output = match name {
                                    "begin" => {
                                        let slot: Slot = serde_json::from_value(args)?;
                                        match live
                                            .uploads
                                            .begin(&store, slot.after, slot.to, slot.archive)
                                            .await?
                                        {
                                            Begun::Upload(upload) => json!({"upload": upload}),
                                            Begun::Result(result) => {
                                                json!({"result": put_result(result)})
                                            }
                                        }
                                    }
                                    "append" => {
                                        let chunk: Chunk = serde_json::from_value(args)?;
                                        live.uploads
                                            .append(&store, &chunk.upload, chunk.bytes)
                                            .await?;
                                        Value::Null
                                    }
                                    "finish" => {
                                        let id: UploadId = serde_json::from_value(args)?;
                                        put_result(live.uploads.finish(&store, &id.upload).await?)
                                    }
                                    _ => {
                                        let id: UploadId = serde_json::from_value(args)?;
                                        live.uploads.abort(&store, &id.upload).await;
                                        Value::Null
                                    }
                                };
                                Ok(output.into())
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

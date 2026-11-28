//! A session worker's bootstrap (the process a runner starts): everything that must exist before
//! plugins load. It waits for its start message, makes its private directory, creates the host, loads
//! the worker facets, reports `ready`, then applies reloads. `WorkerRuntime::close` (the runner's
//! worker facet calls it once the worker idled) closes its host and says `closed` before it exits.
//! On `shutdown` or a closed channel it closes its host and exits.
//!
//! The channel is the worker's stdio, one JSON envelope per line ([`crate::api::v1::control`]).

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use pi_rs_chord::{
    context::Context,
    host::{Host, HostOptions, PluginFacet, create_host},
    link::Link,
    remote::{LinkRole, RemoteOptions},
    sqlite::{Options as SqliteOptions, SqliteDatabase},
    store::StoreEngine,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, watch},
};

use crate::{
    api::v1::control::{
        CloseReason, FromWorker, IpcFrame, IpcPipeLink, ToWorker, read_ipc_frame, worker_control,
        worker_message, write_ipc_frame,
    },
    api::v1::session_address,
    api::v1::worker::worker_outbox_enabled,
    app::worker_principal,
    files::{private_directory, private_store},
    plugins::{runner_worker, sessions},
    principals::Principal,
    registry::WorkerRegistries,
    worker_core,
};

/// How long a shutdown's closes get before the worker exits anyway.
pub const FORCED_EXIT_MS: u64 = 3000;

/// What a worker's bootstrap refused.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("p3 worker: the first message is no start: {0}")]
    NoStart(String),
    #[error("p3 worker: no plugin runtime: {0}")]
    Host(String),
    #[error("p3 worker: no store: {0}")]
    Store(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Files(#[from] crate::files::FileError),
}

/// A session worker's process values, for plugins: what only the base has.
pub struct WorkerRuntime {
    session: String,
    dir: PathBuf,
    idle_ms: u64,
    closed: AtomicBool,
    reason: std::sync::Mutex<Option<CloseReason>>,
    closing: watch::Sender<Option<CloseReason>>,
}

impl WorkerRuntime {
    pub fn new(session: impl Into<String>, dir: PathBuf, idle_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            session: session.into(),
            dir,
            idle_ms,
            closed: AtomicBool::new(false),
            reason: std::sync::Mutex::new(None),
            closing: watch::Sender::new(None),
        })
    }

    /// The session's id.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// The worker's private directory (its store lives in it).
    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    /// How long the worker may idle before it stops itself.
    pub fn idle_ms(&self) -> u64 {
        self.idle_ms
    }

    /// Whether the worker was asked to close.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Stops the worker: records why, and wakes whoever waits for the close. The bootstrap closes the
    /// host and exits.
    pub fn close(&self, reason: CloseReason) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            *self
                .reason
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason);
            self.closing.send_replace(Some(reason));
        }
    }

    /// Waits until the worker is asked to close, and says why.
    pub async fn wait_closed(&self) -> CloseReason {
        let mut closing = self.closing.subscribe();
        loop {
            if let Some(reason) = *closing.borrow_and_update() {
                return reason;
            }
            if closing.changed().await.is_err() {
                return CloseReason::Idle;
            }
        }
    }
}

/// What the worker's host is made of, once it loaded: the facets it may reload later.
pub struct WorkerHost {
    pub host: Host,
    pub facets: Vec<PluginFacet>,
}

/// Runs this process as a session worker on `reader`/`writer`; returns the exit code.
pub async fn run_worker<R, W>(mut reader: R, mut writer: W) -> Result<u8, WorkerError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let first: Option<ToWorker> = match read_ipc_frame(&mut reader).await {
        Ok(IpcFrame::Control(bytes)) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| worker_control::<ToWorker>(&value)),
        Ok(IpcFrame::Binary(_)) => None,
        Err(_) => None,
    };
    let Some(ToWorker::Start {
        session,
        dir,
        idle_ms,
        scripts: _,
    }) = first
    else {
        return Err(WorkerError::NoStart(match first {
            Some(message) => format!("{message:?}"),
            None => "the channel closed".into(),
        }));
    };
    let dir = PathBuf::from(dir);
    private_directory(&dir)?;
    let store_path = dir.join("store.sqlite");
    private_store(&store_path)?;
    // Backup is on for this session (the backup plugin's marker): the store records an outbox,
    // which the plugin uploads. Without the marker the outbox is off, its pending commits dropped.
    let outbox = worker_outbox_enabled(&dir);
    let db = SqliteDatabase::open(&store_path, SqliteOptions::default())
        .await
        .map_err(|error| WorkerError::Store(error.to_string()))?;
    let store = StoreEngine::open_with_outbox(db, outbox)
        .await
        .map_err(|error| WorkerError::Store(error.to_string()))?;
    let (out_tx, mut out_rx) = mpsc::channel::<IpcFrame>(64);
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<ToWorker>(32);
    let parent_link = IpcPipeLink::new(out_tx.clone());
    let reader_link = parent_link.clone();
    let writer_link = parent_link.clone();
    let write_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if write_ipc_frame(&mut writer, &frame).await.is_err() {
                break;
            }
        }
        writer_link.ended();
    });
    let read_task = tokio::spawn(async move {
        while let Ok(frame) = read_ipc_frame(&mut reader).await {
            match frame {
                IpcFrame::Binary(bytes) => reader_link.deliver(bytes),
                IpcFrame::Control(bytes) => {
                    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
                        && let Some(message) = worker_control::<ToWorker>(&value)
                        && ctrl_tx.send(message).await.is_err()
                    {
                        break;
                    }
                }
            }
        }
        reader_link.ended();
    });

    let runtime = WorkerRuntime::new(session.clone(), dir, idle_ms);
    let registries = WorkerRegistries::default();
    let lives = crate::api::v1::contribute::PluginLives::new();
    let context = Context::background();
    let host = create_host(HostOptions {
        principal: Some(worker_principal(session.clone()).value()),
        address: Some(pi_rs_chord::json::JsonValue::from_serde(session_address(
            &session,
        ))),
        store: Some(store.clone()),
        ..HostOptions::default()
    })
    .map_err(|error| WorkerError::Host(error.to_string()))?;
    let mut parent_options = RemoteOptions::new(Principal::Server.value());
    parent_options.role = LinkRole::Parent;
    parent_options.trusted = true;
    let parent_peer = host
        .connect(parent_link.clone(), parent_options)
        .map_err(|error| WorkerError::Host(error.to_string()))?;
    let facets = worker_core::facets(&runtime, &registries, &lives)
        .into_iter()
        .chain([
            sessions::worker_session_facet(runtime.clone(), parent_peer.clone()),
            PluginFacet::new(
                "p3-runner",
                runner_worker::facet(runtime.clone(), registries.clone(), &lives),
            ),
        ])
        .collect::<Vec<_>>();
    let report = host
        .load(facets.clone(), &context)
        .await
        .map_err(|error| WorkerError::Host(error.to_string()))?;
    write_control(
        &out_tx,
        &FromWorker::Ready {
            failed: report.failed.iter().map(|f| f.plugin.clone()).collect(),
        },
    )
    .await?;

    let mut closed = None;
    loop {
        tokio::select! {
            message = ctrl_rx.recv() => {
                match message {
                    Some(ToWorker::Reload { scripts: _ }) => {
                        // Native facets cannot be replaced in a running process: a reload re-runs
                        // the same facet instances, so nothing fails and nothing changes.
                        match host.reload(facets.clone(), &context).await {
                            Ok(report) => {
                                let failed = report
                                    .failed
                                    .iter()
                                    .map(|failure| failure.plugin.clone())
                                    .collect::<Vec<_>>();
                                write_control(&out_tx, &FromWorker::Reloaded { failed }).await?;
                            }
                            Err(error) => {
                                write_control(
                                    &out_tx,
                                    &FromWorker::ReloadFailed {
                                        error: error.to_string(),
                                    },
                                )
                                .await?;
                            }
                        }
                    }
                    Some(ToWorker::Shutdown) | None => break,
                    Some(ToWorker::Start { .. }) => {
                        write_control(
                            &out_tx,
                            &FromWorker::ReloadFailed {
                                error: "p3 worker: already started".into(),
                            },
                        )
                        .await?;
                    }
                }
            }
            reason = runtime.wait_closed() => {
                closed = Some(reason);
                break;
            }
        }
    }
    if let Some(reason) = closed {
        // The server hears why while the channel still stands, then the host closes.
        write_control(&out_tx, &FromWorker::Closed { reason }).await?;
    }
    parent_peer.close();
    parent_link.close();
    lives.end_all();
    host.close(&context)
        .await
        .map_err(|error| WorkerError::Host(error.to_string()))?;
    let _ = store.database().close().await;
    drop(out_tx);
    let _ =
        tokio::time::timeout(std::time::Duration::from_millis(FORCED_EXIT_MS), write_task).await;
    read_task.abort();
    Ok(0)
}

async fn write_control<M: serde::Serialize>(
    outgoing: &mpsc::Sender<IpcFrame>,
    message: &M,
) -> Result<(), WorkerError> {
    let bytes = serde_json::to_vec(&worker_message(message))
        .map_err(|error| WorkerError::Io(std::io::Error::other(error)))?;
    outgoing
        .send(IpcFrame::Control(bytes))
        .await
        .map_err(|_| WorkerError::Io(std::io::Error::other("worker output closed")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_a_worker_records_why_once() {
        let runtime = WorkerRuntime::new("s1", PathBuf::from("/tmp/w"), 50);
        assert!(!runtime.is_closed());
        runtime.close(CloseReason::Idle);
        assert!(runtime.is_closed());
        assert_eq!(
            *runtime
                .reason
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Some(CloseReason::Idle)
        );
    }

    #[tokio::test]
    async fn waiting_for_a_close_returns_its_reason() {
        let runtime = WorkerRuntime::new("s1", PathBuf::from("/tmp/w"), 50);
        let waiter = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.wait_closed().await }
        });
        tokio::task::yield_now().await;
        runtime.close(CloseReason::Idle);
        assert_eq!(waiter.await.expect("waiter"), CloseReason::Idle);
    }
}

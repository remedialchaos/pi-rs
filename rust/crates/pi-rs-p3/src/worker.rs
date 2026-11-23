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
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::watch,
};

use crate::{
    api::v1::control::{CloseReason, FromWorker, ToWorker, decode_line, encode_line},
    api::v1::session_address,
    app::worker_principal,
    files::private_directory,
    plugins::runner_worker,
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
pub async fn run_worker<R, W>(reader: R, writer: W) -> Result<u8, WorkerError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    let mut writer = writer;
    let first: Option<ToWorker> = read_message(&mut lines).await?;
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
    let runtime = WorkerRuntime::new(session.clone(), dir, idle_ms);
    let registries = WorkerRegistries::default();
    let lives = crate::api::v1::contribute::PluginLives::new();
    let context = Context::background();
    let host = create_host(HostOptions {
        principal: Some(worker_principal(session.clone()).value()),
        address: Some(pi_rs_chord::json::JsonValue::from_serde(session_address(
            &session,
        ))),
        ..HostOptions::default()
    })
    .map_err(|error| WorkerError::Host(error.to_string()))?;
    let facets = worker_core::facets(&runtime, &registries, &lives)
        .into_iter()
        .chain(std::iter::once(PluginFacet::new(
            "p3-runner",
            runner_worker::facet(runtime.clone(), registries.clone(), &lives),
        )))
        .collect::<Vec<_>>();
    let report = host
        .load(facets.clone(), &context)
        .await
        .map_err(|error| WorkerError::Host(error.to_string()))?;
    write_message(
        &mut writer,
        &FromWorker::Ready {
            failed: report.failed.iter().map(|f| f.plugin.clone()).collect(),
        },
    )
    .await?;

    let mut closed = None;
    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line? {
                    Some(line) => match decode_line::<ToWorker>(line.as_bytes()) {
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
                                    write_message(&mut writer, &FromWorker::Reloaded { failed }).await?;
                                }
                                Err(error) => {
                                    write_message(
                                        &mut writer,
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
                            write_message(
                                &mut writer,
                                &FromWorker::ReloadFailed {
                                    error: "p3 worker: already started".into(),
                                },
                            )
                            .await?;
                        }
                    },
                    None => break,
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
        write_message(&mut writer, &FromWorker::Closed { reason }).await?;
    }
    lives.end_all();
    host.close(&context)
        .await
        .map_err(|error| WorkerError::Host(error.to_string()))?;
    Ok(0)
}

async fn read_message<R, M>(
    lines: &mut tokio::io::Lines<BufReader<R>>,
) -> Result<Option<M>, WorkerError>
where
    R: AsyncRead + Unpin,
    M: serde::de::DeserializeOwned,
{
    match lines.next_line().await? {
        Some(line) => Ok(decode_line(line.as_bytes())),
        None => Ok(None),
    }
}

async fn write_message<M: serde::Serialize, W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &M,
) -> Result<(), WorkerError> {
    writer.write_all(&encode_line(message)).await?;
    writer.flush().await?;
    Ok(())
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

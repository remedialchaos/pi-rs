//! The local runner with real worker processes: a worker starts as its own process, stops cleanly, a
//! crashed one fails its session until retry, an idle one stops itself, and a rollout reaches a
//! running worker.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use pi_rs_chord::context::Context;
use pi_rs_p3::{
    install::WorkerSpec,
    runner::{
        LocalRunner, ProcessLauncher, RunnerError, RunnerFuture, RunnerOptions, WorkerEnvironment,
        WorkerLaunch, WorkerLauncher, WorkerRunner, WorkerState, WorkerTransport,
    },
};

struct Environment {
    data_dir: PathBuf,
    idle_ms: u64,
}

impl WorkerEnvironment for Environment {
    fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn worker_spec(&self) -> &WorkerSpec {
        static SPEC: std::sync::OnceLock<WorkerSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| WorkerSpec::at(env!("CARGO_BIN_EXE_p3")))
    }

    fn worker_idle_ms(&self) -> u64 {
        self.idle_ms
    }

    fn worker_scripts(&self, _context: &Context) -> RunnerFuture<Vec<String>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
}

/// The real process launcher, keeping each transport so a test can kill a worker.
struct RecordingLauncher {
    inner: ProcessLauncher,
    transports: Mutex<Vec<Arc<dyn WorkerTransport>>>,
}

impl RecordingLauncher {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: ProcessLauncher,
            transports: Mutex::new(Vec::new()),
        })
    }

    fn last(&self) -> Option<Arc<dyn WorkerTransport>> {
        self.transports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
    }
}

impl WorkerLauncher for RecordingLauncher {
    fn launch(&self, launch: WorkerLaunch) -> Result<Arc<dyn WorkerTransport>, RunnerError> {
        let transport = self.inner.launch(launch)?;
        self.transports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(transport.clone());
        Ok(transport)
    }
}

fn runner(idle_ms: u64, data_dir: &Path) -> (Arc<RecordingLauncher>, Arc<LocalRunner>) {
    let launcher = RecordingLauncher::new();
    let runner = Arc::new(LocalRunner::new(
        launcher.clone(),
        RunnerOptions {
            start_timeout: Duration::from_secs(20),
            stop_grace: Duration::from_secs(5),
        },
    ));
    runner.set_environment(Arc::new(Environment {
        data_dir: data_dir.to_path_buf(),
        idle_ms,
    }));
    (launcher, runner)
}

async fn state_of(runner: &LocalRunner, session: &str) -> Option<WorkerState> {
    runner.state().get(session).cloned()
}

async fn wait_for(
    runner: &LocalRunner,
    session: &str,
    done: impl Fn(Option<&WorkerState>) -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if done(runner.state().get(session)) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn a_worker_starts_as_a_process_and_stops_cleanly() {
    let root = tempfile::tempdir().expect("temp dir");
    let (_launcher, runner) = runner(60_000, root.path());
    let context = Context::background();
    let link = runner.start("s1", &context).await.expect("a worker");
    assert!(link.pid > 0);
    assert!(matches!(
        state_of(&runner, "s1").await,
        Some(WorkerState::Running { pid }) if pid == link.pid
    ));
    runner.stop("s1").await.expect("stop");
    assert!(wait_for(&runner, "s1", |state| state.is_none()).await);
    assert!(
        runner.stop("s2").await.is_ok(),
        "stopping no worker is fine"
    );
    runner.close().await.expect("close");
}

#[tokio::test]
async fn a_worker_that_stops_itself_idle_starts_again_on_the_next_start() {
    let root = tempfile::tempdir().expect("temp dir");
    let (_launcher, runner) = runner(120, root.path());
    let context = Context::background();
    let first = runner.start("s1", &context).await.expect("a worker");
    assert!(
        wait_for(&runner, "s1", |state| state.is_none()).await,
        "the worker stopped itself"
    );
    let second = runner.start("s1", &context).await.expect("a worker again");
    assert_ne!(first.pid, second.pid);
    runner.stop("s1").await.expect("stop");
    runner.close().await.expect("close");
}

#[tokio::test]
async fn a_crashed_worker_fails_its_session_until_retry() {
    let root = tempfile::tempdir().expect("temp dir");
    let (launcher, runner) = runner(60_000, root.path());
    let context = Context::background();
    let first = runner.start("s1", &context).await.expect("a worker");
    launcher
        .last()
        .expect("the worker's transport")
        .kill()
        .await
        .expect("kill");
    assert!(
        wait_for(&runner, "s1", |state| {
            matches!(state, Some(WorkerState::Failed { .. }))
        })
        .await,
        "the crash failed the session"
    );
    match runner.start("s1", &context).await {
        Err(RunnerError::Failed { reason, .. }) => assert!(reason.contains("exited")),
        other => panic!("a failed session does not start: {other:?}"),
    }
    runner.retry("s1");
    let again = runner.start("s1", &context).await.expect("a new worker");
    assert_ne!(first.pid, again.pid);
    runner.stop("s1").await.expect("stop");
    runner.close().await.expect("close");
}

#[tokio::test]
async fn a_rollout_reaches_a_running_worker_and_clears_failures() {
    let root = tempfile::tempdir().expect("temp dir");
    let (_launcher, runner) = runner(60_000, root.path());
    let context = Context::background();
    runner.start("s1", &context).await.expect("a worker");
    let step = runner
        .rollout(&["p3-sessions@1".to_string()], &context)
        .await
        .expect("rollout");
    assert_eq!(step.name, "workers");
    assert_eq!(
        step.hosts,
        [pi_rs_p3::runner::RolloutHost::Reloaded {
            host: "s1".into(),
            failed: Vec::new(),
        }]
    );
    // Nothing changed: a second rollout with the same scripts reaches nobody.
    let step = runner
        .rollout(&["p3-sessions@1".to_string()], &context)
        .await
        .expect("rollout");
    assert!(step.hosts.is_empty());
    runner.stop("s1").await.expect("stop");
    runner.close().await.expect("close");
}

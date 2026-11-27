//! The background server's supervisor (`p3` and `p3 restart` start it, detached): it runs the server
//! as its child, starts it again after a crash, and ends with it when it stops. One per data
//! directory: it holds the supervisor's lock and writes `supervisor-running.json` (its pid), so
//! `p3 stop` finds it also while no server runs between two starts.
//!
//! A child that never listened failed its start (a busy port, another server on the directory): the
//! supervisor ends with its exit code instead of trying again for ever. A child that exits 0 was
//! stopped. Anything else after the listen is a crash.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use pi_rs_chord::node::workers::ProcessLock;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
};

use crate::{
    client::{self, Request},
    files::private_directory,
    install::SUPERVISED_ENV,
    records::{remove_supervisor_record, running, supervisor_lock, write_supervisor_record},
};

/// The wait before a crashed server starts again: 1 s, doubled for each crash in a row, at most 30 s.
pub const FIRST_DELAY_MS: u64 = 1000;
pub const LONGEST_DELAY_MS: u64 = 30_000;

/// A server that ran this long was no crash in a row: the next wait is the first one again.
pub const SETTLED_MS: u64 = 60_000;

/// How long a asked-to-stop server gets before it is killed.
const STOP_GRACE_MS: u64 = 10_000;

/// Runs the server `command` on `data_dir` until it is stopped, starting it again after each crash.
pub async fn supervise(data_dir: PathBuf, command: Vec<OsString>) -> i32 {
    let lock = match ProcessLock::acquire(&supervisor_lock(&data_dir), Duration::from_millis(1000))
        .await
    {
        Ok(lock) => lock,
        Err(_) => return 1,
    };
    if private_directory(&data_dir.join("logs")).is_err() {
        return 1;
    }
    if write_supervisor_record(&data_dir, std::process::id()).is_err() {
        return 1;
    }
    let log_path = data_dir.join("logs").join("server.log");
    let mut signals = Signals::new();
    let mut delay = FIRST_DELAY_MS;
    let mut code;
    loop {
        let started = Instant::now();
        let mut child = match spawn(&command, &log_path) {
            Ok(child) => child,
            Err(error) => {
                say(&log_path, &format!("the server did not start: {error}")).await;
                code = 1;
                break;
            }
        };
        let listened = Arc::new(AtomicBool::new(false));
        let reader = read_output(child.stdout.take(), log_path.clone(), listened.clone());
        let stopped = tokio::select! {
            status = child.wait() => Stopped::Exited(status.ok().and_then(|status| status.code())),
            _ = signals.next() => Stopped::Asked,
        };
        let exit = match stopped {
            Stopped::Asked => {
                ask_stop(&data_dir, &mut child).await;
                code = 0;
                break;
            }
            Stopped::Exited(exit) => exit,
        };
        let _ = reader.await;
        let exit = exit.unwrap_or(1);
        code = exit;
        if exit == 0 || !listened.load(Ordering::Acquire) {
            break;
        }
        if started.elapsed() >= Duration::from_millis(SETTLED_MS) {
            delay = FIRST_DELAY_MS;
        }
        say(
            &log_path,
            &format!("the server exited with {exit}; starting it again in {delay} ms"),
        )
        .await;
        let waiting = tokio::time::sleep(Duration::from_millis(delay));
        tokio::pin!(waiting);
        let asked = tokio::select! {
            _ = &mut waiting => false,
            _ = signals.next() => true,
        };
        if asked {
            code = 0;
            break;
        }
        delay = (delay * 2).min(LONGEST_DELAY_MS);
    }
    remove_supervisor_record(&data_dir);
    drop(lock);
    code
}

enum Stopped {
    Asked,
    Exited(Option<i32>),
}

#[cfg(unix)]
struct Signals {
    terminate: Option<tokio::signal::unix::Signal>,
    interrupt: Option<tokio::signal::unix::Signal>,
}

#[cfg(unix)]
impl Signals {
    fn new() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        Self {
            terminate: signal(SignalKind::terminate()).ok(),
            interrupt: signal(SignalKind::interrupt()).ok(),
        }
    }

    async fn next(&mut self) {
        let terminate = self.terminate.as_mut();
        let interrupt = self.interrupt.as_mut();
        tokio::select! {
            _ = async move {
                match terminate {
                    Some(terminate) => {
                        terminate.recv().await;
                    }
                    None => std::future::pending().await,
                }
            } => {}
            _ = async move {
                match interrupt {
                    Some(interrupt) => {
                        interrupt.recv().await;
                    }
                    None => std::future::pending().await,
                }
            } => {}
        }
    }
}

#[cfg(not(unix))]
struct Signals;

#[cfg(not(unix))]
impl Signals {
    fn new() -> Self {
        Self
    }

    async fn next(&mut self) {
        std::future::pending().await
    }
}

fn spawn(command: &[OsString], log_path: &Path) -> Result<Child, std::io::Error> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(log_path, std::fs::Permissions::from_mode(0o600))?;
    }
    let errors = log.try_clone()?;
    Command::new(&command[0])
        .args(&command[1..])
        .env(SUPERVISED_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(errors))
        .kill_on_drop(true)
        .spawn()
}

async fn read_output(
    stdout: Option<tokio::process::ChildStdout>,
    log_path: PathBuf,
    listened: Arc<AtomicBool>,
) -> Result<(), std::io::Error> {
    let Some(stdout) = stdout else {
        return Ok(());
    };
    let mut log = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .await?;
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        if line.contains("p3 on ") {
            listened.store(true, Ordering::Release);
        }
        log.write_all(format!("{line}\n").as_bytes()).await?;
        log.flush().await?;
    }
    Ok(())
}

async fn say(log_path: &Path, text: &str) {
    let stamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if let Ok(mut log) = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .await
    {
        let _ = log
            .write_all(format!("{stamp} supervisor: {text}\n").as_bytes())
            .await;
        let _ = log.flush().await;
    }
}

/// Asks the server to stop as the owner; one that does not answer, or does not leave, is killed.
async fn ask_stop(data_dir: &Path, child: &mut Child) {
    if let Some(record) = running(data_dir) {
        let dir = data_dir.to_path_buf();
        let asked = tokio::task::spawn_blocking(move || {
            let token = std::fs::read_to_string(crate::plugins::sign_in::owner_token_file(&dir))
                .map_err(|error| error.to_string())?
                .trim()
                .to_string();
            client::post(
                record.port,
                crate::plugins::control::STOP_PATH,
                Request {
                    bearer: Some(&token),
                    ..Request::default()
                },
            )
            .map(|answer| answer.status)
            .map_err(|error| error.to_string())
        })
        .await
        .ok()
        .and_then(Result::ok);
        if asked.is_some_and(|status| (200..300).contains(&status))
            && tokio::time::timeout(Duration::from_millis(STOP_GRACE_MS), child.wait())
                .await
                .is_ok()
        {
            return;
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// The tests run supervisors in this process, and a signal reaches every listener in it: one at a
    /// time.
    static SIGNALS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn count(path: &Path) -> usize {
        std::fs::read_to_string(path)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    async fn wait_until(counted: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if counted() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    use crate::unix_signal::signal;

    #[tokio::test]
    async fn a_crashed_server_starts_again() {
        let _serialized = SIGNALS.lock().await;
        let root = tempfile::tempdir().expect("temp dir");
        let counter = root.path().join("counter");
        let script = format!(
            "echo x >> {counter}; echo 'p3 on a crash'; exit 1",
            counter = counter.display()
        );
        let supervisor = tokio::spawn({
            let data_dir = root.path().to_path_buf();
            async move {
                supervise(
                    data_dir,
                    vec![
                        OsString::from("/bin/sh"),
                        OsString::from("-c"),
                        OsString::from(script),
                    ],
                )
                .await
            }
        });
        let restarted = wait_until(|| count(&counter) >= 2).await;
        if !restarted {
            let log = std::fs::read_to_string(root.path().join("logs").join("server.log"))
                .unwrap_or_default();
            panic!("the supervisor did not start the crashed server again; log:\n{log}");
        }
        let pid = std::fs::read_to_string(root.path().join("supervisor-running.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|value| value.get("pid")?.as_u64())
            .map(|pid| pid as u32);
        if let Some(pid) = pid {
            signal(pid, "TERM");
        }
        let code = tokio::time::timeout(Duration::from_secs(5), supervisor)
            .await
            .expect("the supervisor ends")
            .expect("joined");
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn a_stopped_supervisor_stops_its_child_and_ends_clean() {
        let _serialized = SIGNALS.lock().await;
        let root = tempfile::tempdir().expect("temp dir");
        let flag = root.path().join("listening");
        let script = format!(
            "echo 'p3 on a server'; touch {flag}; sleep 30",
            flag = flag.display()
        );
        let supervisor = tokio::spawn({
            let data_dir = root.path().to_path_buf();
            async move {
                supervise(
                    data_dir,
                    vec![
                        OsString::from("/bin/sh"),
                        OsString::from("-c"),
                        OsString::from(script),
                    ],
                )
                .await
            }
        });
        assert!(wait_until(|| flag.exists()).await, "the server listened");
        let pid = std::fs::read_to_string(root.path().join("supervisor-running.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|value| value.get("pid")?.as_u64())
            .map(|pid| pid as u32)
            .expect("supervisor pid");
        signal(pid, "TERM");
        let code = tokio::time::timeout(Duration::from_secs(10), supervisor)
            .await
            .expect("the supervisor ends")
            .expect("joined");
        assert_eq!(code, 0);
        assert!(!root.path().join("supervisor-running.json").exists());
    }

    #[test]
    fn the_first_restart_wait_doubles_to_a_cap() {
        let mut delay = FIRST_DELAY_MS;
        for _ in 0..10 {
            delay = (delay * 2).min(LONGEST_DELAY_MS);
        }
        assert_eq!(delay, LONGEST_DELAY_MS);
    }
}

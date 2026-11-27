use crate::{
    Error, Result, data_backup, dev_control, http, login_environment, private_directory,
    server_config::{Environment, Mode, Preferences, ServerConfig, Source},
    server_scan,
};
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::{Duration, Instant},
};
use tokio::{
    process::{Child, Command},
    sync::watch,
};

pub const PROBE_INTERVAL: Duration = Duration::from_millis(1500);
pub const STOP_GRACE: Duration = Duration::from_secs(15);
const READY_LIMIT: Duration = Duration::from_secs(90);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase", tag = "phase", content = "message")]
pub enum Phase {
    Checking,
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed(String),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Ownership {
    None,
    Managed,
    External,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct State {
    #[serde(flatten)]
    pub phase: Phase,
    pub ownership: Ownership,
}

enum Managed {
    Child(Child),
    Adopted { pid: i32, identity: String },
}

pub struct ServerController {
    pub config: ServerConfig,
    pub log_file: PathBuf,
    pid_file: PathBuf,
    changes: watch::Sender<State>,
    managed: Option<Managed>,
    started: Option<Instant>,
    grace: Duration,
    env: Option<Environment>,
    inherited: Environment,
    health: reqwest::Client,
    upgrade: Option<String>,
}
impl ServerController {
    pub fn new(config: ServerConfig, home: &Path, inherited: Environment) -> Result<Self> {
        let log_file = home.join(format!("Library/Logs/Pi/server-{}.log", config.port));
        let pid_file = home.join(format!(
            "Library/Application Support/Pi/server-{}.pid",
            config.port
        ));
        let (changes, _) = watch::channel(State {
            phase: Phase::Checking,
            ownership: Ownership::None,
        });
        Ok(Self {
            config,
            log_file,
            pid_file,
            changes,
            managed: None,
            started: None,
            grace: STOP_GRACE,
            env: None,
            inherited,
            health: http(Duration::from_secs(1))?,
            upgrade: None,
        })
    }
    pub fn with_stop_grace(mut self, duration: Duration) -> Self {
        self.grace = duration;
        self
    }
    pub fn state(&self) -> State {
        self.changes.borrow().clone()
    }
    pub fn changes(&self) -> watch::Receiver<State> {
        self.changes.subscribe()
    }
    fn set(&self, phase: Phase, ownership: Ownership) {
        self.changes.send_if_modified(|current| {
            if current.phase == phase && current.ownership == ownership {
                return false;
            }
            *current = State { phase, ownership };
            true
        });
    }
    pub fn prepare_upgrade(&mut self, build: &str, preferences: &mut dyn Preferences) {
        if self.config.runs_bundled {
            self.upgrade =
                match data_backup::before_upgrade(&self.config.data_dir, build, preferences) {
                    Ok(line) => line,
                    Err(error) => Some(format!("data backup failed: {error}")),
                };
        }
    }
    pub fn can_start(&self) -> bool {
        self.config.source == Source::Own
            && self.managed.is_none()
            && self.changes.borrow().ownership != Ownership::External
            && matches!(
                self.changes.borrow().phase,
                Phase::Checking | Phase::Stopped | Phase::Failed(_)
            )
    }
    pub fn can_stop(&self) -> bool {
        let state = self.changes.borrow();
        state.ownership == Ownership::Managed && state.phase != Phase::Stopping
    }
    pub fn can_restart(&self) -> bool {
        let state = self.changes.borrow();
        state.phase == Phase::Running
            && (state.ownership == Ownership::Managed
                || dev_control::available(&self.config.data_dir))
    }
    pub async fn poll(&mut self) -> Result<()> {
        let healthy = server_scan::healthy(&self.health, &self.config.origin()).await;
        if healthy
            && self.managed.is_none()
            && self.config.source == Source::Own
            && let Some(pid) = fs::read_to_string(&self.pid_file)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
                .filter(|&pid| pid > 0)
            && server_scan::listens(pid, self.config.port).await
            && let Some(identity) = identity(pid).await
        {
            self.managed = Some(Managed::Adopted { pid, identity });
        }
        if let Some(code) = self.exit_code().await? {
            self.exited();
            self.set(
                Phase::Failed(format!("The server exited with status {code}; see its log")),
                Ownership::None,
            );
            return Ok(());
        }
        if self.managed.is_some() {
            if self.state().phase == Phase::Stopping {
                return Ok(());
            }
            let phase = if healthy {
                Phase::Running
            } else if self.started.is_some_and(|at| at.elapsed() > READY_LIMIT) {
                Phase::Failed("The server did not answer within 90 seconds".into())
            } else {
                Phase::Starting
            };
            self.set(phase, Ownership::Managed);
        } else if healthy {
            self.set(Phase::Running, Ownership::External);
        } else if !matches!(self.state().phase, Phase::Failed(_)) {
            self.set(Phase::Stopped, Ownership::None);
        }
        Ok(())
    }
    pub async fn start(&mut self) -> Result<()> {
        if !self.can_start() {
            return Ok(());
        }
        self.set(Phase::Starting, Ownership::Managed);
        if let Err(error) = self.launch().await {
            self.set(Phase::Failed(error.to_string()), Ownership::None);
            return Err(error);
        }
        Ok(())
    }
    async fn launch(&mut self) -> Result<()> {
        if self.env.is_none() {
            self.env = Some(
                login_environment::capture(
                    &login_environment::login_shell(&self.inherited),
                    &self.inherited,
                )
                .await,
            );
        }
        let env = self
            .env
            .as_ref()
            .ok_or_else(|| Error::Protocol("could not read login environment".into()))?;
        let binary = self
            .config
            .binary
            .clone()
            .or_else(|| login_environment::which("p2", env.get("PATH").map(String::as_str)))
            .ok_or_else(|| Error::Protocol("No p2 binary on the login shell's PATH".into()))?;
        for file in [&self.log_file, &self.pid_file] {
            if let Some(parent) = file.parent() {
                private_directory(parent)?;
            }
        }
        private_directory(&self.config.data_dir)?;
        let mut log = open_log(&self.log_file)?;
        if let Some(backup) = self.upgrade.take() {
            writeln!(log, "\n--- {backup}")?;
        }
        writeln!(
            log,
            "\n--- starting {} in {}",
            binary.display(),
            self.config.data_dir.display()
        )?;
        let mut cmd = Command::new(&binary);
        if self.config.mode == Mode::Dev {
            cmd.arg("--dev");
        }
        cmd.env_clear()
            .envs(env)
            .env("PORT", self.config.port.to_string())
            .env("P2_DATA_DIR", &self.config.data_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .kill_on_drop(true);
        if self.config.fake {
            cmd.env("P2_FAKE", "1");
        }
        let child = cmd
            .spawn()
            .map_err(|e| Error::Protocol(format!("Could not start p2: {e}")))?;
        if let Some(pid) = child.id() {
            let _ = fs::write(&self.pid_file, pid.to_string());
        }
        self.managed = Some(Managed::Child(child));
        self.started = Some(Instant::now());
        Ok(())
    }
    pub async fn stop(&mut self) -> Result<()> {
        if self.managed.is_none() {
            return Ok(());
        }
        self.set(Phase::Stopping, Ownership::Managed);
        if self.exit_code().await?.is_none() {
            self.signal(false).await?;
            let deadline = Instant::now() + self.grace;
            loop {
                if self.exit_code().await?.is_some() {
                    break;
                }
                if Instant::now() >= deadline {
                    self.signal(true).await?;
                    if let Some(Managed::Child(child)) = &mut self.managed {
                        child.wait().await?;
                    }
                    break;
                }
                tokio::time::sleep(
                    Duration::from_millis(40)
                        .min(deadline.saturating_duration_since(Instant::now())),
                )
                .await;
            }
        }
        self.exited();
        self.set(Phase::Stopped, Ownership::None);
        Ok(())
    }
    pub async fn restart(&mut self) -> Result<()> {
        if self.managed.is_some() {
            self.stop().await?;
            return self.start().await;
        }
        if !dev_control::available(&self.config.data_dir) {
            return Ok(());
        }
        self.set(Phase::Starting, Ownership::External);
        let restarted = dev_control::restart(&self.config.data_dir).await;
        self.poll().await?;
        restarted.map(|_| ())
    }
    async fn signal(&mut self, hard: bool) -> Result<()> {
        let pid = match self.managed.as_mut() {
            Some(Managed::Child(child)) if hard => {
                child.start_kill()?;
                return Ok(());
            }
            Some(Managed::Child(child)) => child.id().and_then(|pid| i32::try_from(pid).ok()),
            Some(Managed::Adopted {
                pid,
                identity: expected,
            }) => (identity(*pid).await.as_ref() == Some(expected)).then_some(*pid),
            None => None,
        };
        if let Some(pid) = pid {
            send_signal(pid, hard)?;
        }
        Ok(())
    }
    async fn exit_code(&mut self) -> Result<Option<i32>> {
        Ok(match &mut self.managed {
            Some(Managed::Child(child)) => child.try_wait()?.map(status_code),
            Some(Managed::Adopted {
                pid,
                identity: expected,
            }) => (identity(*pid).await.as_ref() != Some(expected)).then_some(0),
            None => None,
        })
    }
    fn exited(&mut self) {
        self.managed = None;
        self.started = None;
        let _ = fs::remove_file(&self.pid_file);
    }
}

fn status_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.code().or_else(|| status.signal()).unwrap_or(1)
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(1)
    }
}
fn open_log(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
fn send_signal(pid: i32, hard: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use nix::{
            errno::Errno,
            sys::signal::{Signal, kill},
            unistd::Pid,
        };
        match kill(
            Pid::from_raw(pid),
            if hard {
                Signal::SIGKILL
            } else {
                Signal::SIGTERM
            },
        ) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(io::Error::from_raw_os_error(error as i32)),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, hard);
        Ok(())
    }
}

#[cfg(target_os = "linux")]
async fn identity(pid: i32) -> Option<String> {
    let data = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = data.rsplit_once(')')?.1.split_ascii_whitespace();
    if fields.next()? == "Z" {
        return None;
    }
    fields.nth(18).map(str::to_owned)
}
#[cfg(all(unix, not(target_os = "linux")))]
async fn identity(pid: i32) -> Option<String> {
    let output = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "state=", "-o", "lstart="])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (state, created) = text.trim().split_once(' ')?;
    (!state.starts_with('Z')).then(|| created.trim().into())
}
#[cfg(not(unix))]
async fn identity(_: i32) -> Option<String> {
    None
}

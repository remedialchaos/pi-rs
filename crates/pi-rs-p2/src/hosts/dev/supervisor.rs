//! Native Unix dev supervisor that respawns `p2 --dev` when it exits with `DEV_RESTART_EXIT_CODE`.

use std::{io, path::PathBuf, process::Stdio, time::Duration};

use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::{oneshot, watch},
    task::JoinHandle,
    time::timeout,
};

use super::{DEV_REBUILD_CMD_ENV, DEV_RESTART_EXIT_CODE, DEV_SUPERVISOR_ENV};

#[derive(Clone, Debug)]
pub struct SupervisorOptions {
    pub binary: PathBuf,
    pub data_dir: PathBuf,
    pub port: u16,
    pub fake: bool,
    pub rebuild_cmd: Option<String>,
    pub extra_env: Vec<(String, String)>,
    pub extra_args: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SupervisorState {
    pub generation: u64,
    pub pid: Option<u32>,
    pub address: Option<String>,
    pub running: bool,
    pub last_exit_code: Option<i32>,
}

pub struct DevSupervisor {
    state: watch::Receiver<SupervisorState>,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl DevSupervisor {
    pub async fn start(options: SupervisorOptions) -> io::Result<Self> {
        let mut generation = 1u64;
        let (mut child, address) = spawn_ready(&options).await?;
        let (state_tx, state_rx) = watch::channel(SupervisorState {
            generation,
            pid: child.id(),
            address: Some(address),
            running: true,
            last_exit_code: None,
        });
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stop_rx => {
                        terminate_child(&mut child).await;
                        state_tx.send_modify(|s| {
                            s.running = false;
                            s.pid = None;
                        });
                        break;
                    }
                    waited = child.wait() => {
                        let code = waited.ok().and_then(|status| status.code());
                        if code == Some(i32::from(DEV_RESTART_EXIT_CODE)) {
                            generation += 1;
                            match spawn_ready(&options).await {
                                Ok((next_child, next_addr)) => {
                                    child = next_child;
                                    state_tx.send_replace(SupervisorState {
                                        generation,
                                        pid: child.id(),
                                        address: Some(next_addr),
                                        running: true,
                                        last_exit_code: Some(i32::from(DEV_RESTART_EXIT_CODE)),
                                    });
                                }
                                Err(_) => {
                                    state_tx.send_modify(|s| {
                                        s.running = false;
                                        s.pid = None;
                                        s.last_exit_code = code;
                                    });
                                    break;
                                }
                            }
                        } else {
                            state_tx.send_modify(|s| {
                                s.running = false;
                                s.pid = None;
                                s.last_exit_code = code;
                            });
                            break;
                        }
                    }
                }
            }
        });
        Ok(Self {
            state: state_rx,
            stop: Some(stop_tx),
            task,
        })
    }

    pub fn state(&self) -> watch::Receiver<SupervisorState> {
        self.state.clone()
    }

    pub async fn stop(mut self) -> io::Result<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.await.map_err(io::Error::other)
    }
}

async fn spawn_ready(options: &SupervisorOptions) -> io::Result<(Child, String)> {
    let mut command = Command::new(&options.binary);
    command
        .arg("--dev")
        .args(&options.extra_args)
        .env("P2_DATA_DIR", &options.data_dir)
        .env("P2_DEV", "1")
        .env(DEV_SUPERVISOR_ENV, "1")
        .env("PORT", options.port.to_string())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    if options.fake {
        command.env("P2_FAKE", "1");
    }
    if let Some(rebuild_cmd) = &options.rebuild_cmd {
        command.env(DEV_REBUILD_CMD_ENV, rebuild_cmd);
    }
    for (key, value) in &options.extra_env {
        command.env(key, value);
    }
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing supervised p2 stdout"))?;
    let mut lines = BufReader::new(stdout).lines();
    let ready = timeout(Duration::from_secs(30), async {
        while let Some(line) = lines.next_line().await? {
            if let Some(rest) = line.strip_prefix("p2: ") {
                let addr = rest.split_whitespace().next().unwrap_or(rest).to_owned();
                return Ok::<_, io::Error>(addr);
            }
        }
        Err(io::Error::other(
            "supervised p2 exited before becoming ready",
        ))
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "supervised p2 startup timed out"))?;
    let address = match ready {
        Ok(address) => address,
        Err(error) => {
            terminate_child(&mut child).await;
            return Err(error);
        }
    };
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    Ok((child, address))
}

async fn terminate_child(child: &mut Child) {
    if let Some(pid) = child.id() {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .await;
    }
    if timeout(Duration::from_secs(10), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

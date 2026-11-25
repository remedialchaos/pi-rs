use crate::{Result, dev_control, http};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{process::Command, task::JoinSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FoundServer {
    pub port: u16,
    pub pid: i32,
    pub data_dir: PathBuf,
    pub checkout: Option<PathBuf>,
    pub dev: bool,
}

pub async fn healthy(client: &reqwest::Client, origin: &url::Url) -> bool {
    #[derive(Deserialize)]
    struct Probe {
        name: String,
    }
    let Ok(health) = origin.join("/health") else {
        return false;
    };
    let Ok(response) = client.get(health).send().await else {
        return false;
    };
    response.status() == 200
        && response
            .json::<Probe>()
            .await
            .is_ok_and(|probe| probe.name == "p2")
}

pub async fn scan(default_data_dir: &Path) -> Result<Vec<FoundServer>> {
    let client = http(Duration::from_millis(800))?;
    let listeners = neighbors().await;
    let mut jobs = JoinSet::new();
    for (pid, port) in listeners {
        let client = client.clone();
        let default_data_dir = default_data_dir.to_owned();
        jobs.spawn(async move {
            let origin = url::Url::parse(&format!("http://127.0.0.1:{port}/")).ok()?;
            if !healthy(&client, &origin).await {
                return None;
            }
            let data_dir = process_data_dir(pid).unwrap_or(default_data_dir);
            let checkout = process_cwd(pid).await;
            Some(FoundServer {
                port,
                pid,
                dev: dev_control::available(&data_dir),
                data_dir,
                checkout,
            })
        });
    }
    let mut servers = Vec::new();
    while let Some(job) = jobs.join_next().await {
        if let Ok(Some(server)) = job {
            servers.push(server);
        }
    }
    servers.sort_unstable_by_key(|server| (server.port, server.pid));
    servers.dedup_by_key(|server| server.port);
    Ok(servers)
}

#[cfg(unix)]
async fn neighbors() -> Vec<(i32, u16)> {
    let uid = nix::unistd::getuid().as_raw().to_string();
    let mut listeners = Vec::new();
    if let Some(text) = lsof(&[
        "-nP",
        "-a",
        "-iTCP@127.0.0.1",
        "-sTCP:LISTEN",
        "-u",
        &uid,
        "-c",
        "node",
        "-F",
        "pn",
    ])
    .await
    {
        let mut pid = None;
        for line in text.lines() {
            if let Some(value) = line.strip_prefix('p') {
                pid = value.parse::<i32>().ok().filter(|pid| *pid > 0);
            }
            if let Some(pid) = pid
                && let Some((host, port)) = line
                    .strip_prefix('n')
                    .and_then(|line| line.rsplit_once(':'))
                && matches!(host, "127.0.0.1" | "*")
                && let Ok(port) = port.parse()
            {
                listeners.push((pid, port));
            }
        }
    }
    listeners
}
#[cfg(not(unix))]
async fn neighbors() -> Vec<(i32, u16)> {
    Vec::new()
}

#[cfg(unix)]
pub(crate) async fn listens(pid: i32, port: u16) -> bool {
    let pid_string = pid.to_string();
    lsof(&[
        "-a",
        "-p",
        &pid_string,
        &format!("-iTCP:{port}"),
        "-sTCP:LISTEN",
        "-t",
    ])
    .await
    .is_some_and(|s| s.lines().any(|s| s == pid_string))
}
#[cfg(not(unix))]
pub(crate) async fn listens(_: i32, _: u16) -> bool {
    false
}

#[cfg(unix)]
async fn lsof(args: &[&str]) -> Option<String> {
    let program = if cfg!(target_os = "macos") {
        "/usr/sbin/lsof"
    } else {
        "lsof"
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(2), command.output())
        .await
        .ok()?
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(target_os = "linux")]
fn process_data_dir(pid: i32) -> Option<PathBuf> {
    let bytes = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    use std::os::unix::ffi::OsStrExt;
    let dir = bytes
        .split(|&b| b == 0)
        .find_map(|entry| entry.strip_prefix(b"P2_DATA_DIR="))?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir)))
}
#[cfg(target_os = "macos")]
#[expect(unsafe_code, reason = "Darwin exposes KERN_PROCARGS2 through sysctl")]
fn process_data_dir(pid: i32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut len: usize = 0;
    // SAFETY: mib has exactly three initialized integers. In the size query no output buffer is passed.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || len < 4
    {
        return None;
    }
    let mut data = vec![0u8; len];
    // SAFETY: data's allocation covers len initialized, writable bytes; sysctl cannot grow it.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            data.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let data = data.get(..len)?;
    let (argc, rest) = data.split_first_chunk::<4>()?;
    let argc = usize::try_from(i32::from_ne_bytes(*argc)).ok()?;
    let start = rest.iter().position(|&b| b == 0)?;
    let start = start + rest[start..].iter().position(|&b| b != 0)?;
    let dir = rest[start..]
        .split(|&b| b == 0)
        .skip(argc)
        .find_map(|s| s.strip_prefix(b"P2_DATA_DIR="))?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir)))
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_data_dir(_: i32) -> Option<PathBuf> {
    None
}

async fn process_cwd(pid: i32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
    #[cfg(target_os = "macos")]
    {
        let out = lsof(&["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"]).await?;
        out.lines()
            .find_map(|line| line.strip_prefix('n'))
            .map(PathBuf::from)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

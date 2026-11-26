use crate::{Result, dev_control, http};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::task::JoinSet;
#[cfg(all(unix, not(target_os = "linux")))]
use {std::process::Stdio, tokio::process::Command};

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

#[cfg(target_os = "linux")]
fn parse_proc_ipv4(hex: &str) -> Option<std::net::Ipv4Addr> {
    if hex.len() != 8 {
        return None;
    }
    let raw = u32::from_str_radix(hex, 16).ok()?;
    Some(std::net::Ipv4Addr::from(raw.to_ne_bytes()))
}

#[cfg(target_os = "linux")]
fn parse_proc_ipv6(hex: &str) -> Option<std::net::Ipv6Addr> {
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (idx, chunk) in bytes.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let word = u32::from_str_radix(&hex[idx * 8..(idx + 1) * 8], 16).ok()?;
        *chunk = word.to_ne_bytes();
    }
    Some(std::net::Ipv6Addr::from(bytes))
}

#[cfg(target_os = "linux")]
fn is_loopback_or_wildcard_addr(hex: &str, ipv6: bool) -> bool {
    if ipv6 {
        parse_proc_ipv6(hex).is_some_and(|addr| {
            addr == std::net::Ipv6Addr::LOCALHOST
                || addr == std::net::Ipv6Addr::UNSPECIFIED
                || addr.to_ipv4_mapped().is_some_and(|v4| {
                    v4 == std::net::Ipv4Addr::LOCALHOST || v4 == std::net::Ipv4Addr::UNSPECIFIED
                })
        })
    } else {
        parse_proc_ipv4(hex).is_some_and(|addr| {
            addr == std::net::Ipv4Addr::LOCALHOST || addr == std::net::Ipv4Addr::UNSPECIFIED
        })
    }
}

#[cfg(target_os = "linux")]
fn parse_proc_net_listeners(
    text: &str,
    ipv6: bool,
    uid_filter: Option<u32>,
    loopback_only: bool,
) -> std::collections::HashMap<u64, u16> {
    let mut inodes = std::collections::HashMap::new();
    for line in text.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let _sl = fields.next();
        let Some(local_addr) = fields.next() else {
            continue;
        };
        let _rem = fields.next();
        let Some(state) = fields.next() else {
            continue;
        };
        // 0A == TCP_LISTEN
        if state != "0A" {
            continue;
        }
        let _queues = fields.next();
        let _tr = fields.next();
        let _retrnsmt = fields.next();
        let Some(uid_str) = fields.next() else {
            continue;
        };
        if let Some(expected_uid) = uid_filter
            && uid_str.parse::<u32>().ok() != Some(expected_uid)
        {
            continue;
        }
        let _timeout = fields.next();
        let Some(inode) = fields
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|inode| *inode > 0)
        else {
            continue;
        };
        let Some((ip_hex, port_hex)) = local_addr.split_once(':') else {
            continue;
        };
        if loopback_only && !is_loopback_or_wildcard_addr(ip_hex, ipv6) {
            continue;
        }
        let Some(port) = u16::from_str_radix(port_hex, 16).ok() else {
            continue;
        };
        inodes.insert(inode, port);
    }
    inodes
}

#[cfg(target_os = "linux")]
fn parse_socket_inode(target: &Path) -> Option<u64> {
    let text = target.to_str()?;
    text.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse::<u64>()
        .ok()
}

#[cfg(target_os = "linux")]
fn proc_listening_inodes(
    uid_filter: Option<u32>,
    loopback_only: bool,
) -> std::collections::HashMap<u64, u16> {
    let mut inodes = std::collections::HashMap::new();
    for (path, ipv6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        if let Ok(text) = std::fs::read_to_string(path) {
            inodes.extend(parse_proc_net_listeners(
                &text,
                ipv6,
                uid_filter,
                loopback_only,
            ));
        }
    }
    inodes
}

#[cfg(target_os = "linux")]
async fn neighbors() -> Vec<(i32, u16)> {
    use std::os::unix::fs::MetadataExt;
    let uid = nix::unistd::getuid().as_raw();
    let inodes = proc_listening_inodes(Some(uid), true);
    if inodes.is_empty() {
        return Vec::new();
    }
    let mut listeners = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return listeners;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
            .filter(|pid| *pid > 0)
        else {
            continue;
        };
        let proc_dir = entry.path();
        if std::fs::metadata(&proc_dir).ok().map(|meta| meta.uid()) != Some(uid) {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(proc_dir.join("comm")) else {
            continue;
        };
        if !comm.trim().starts_with("node") {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(proc_dir.join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if let Ok(target) = std::fs::read_link(fd.path())
                && let Some(inode) = parse_socket_inode(&target)
                && let Some(&port) = inodes.get(&inode)
            {
                listeners.push((pid, port));
            }
        }
    }
    listeners
}

#[cfg(all(unix, not(target_os = "linux")))]
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

#[cfg(target_os = "linux")]
pub(crate) async fn listens(pid: i32, port: u16) -> bool {
    let inodes = proc_listening_inodes(None, false);
    if !inodes.values().any(|&p| p == port) {
        return false;
    }
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    for fd in fds.flatten() {
        if let Ok(target) = std::fs::read_link(fd.path())
            && let Some(inode) = parse_socket_inode(&target)
            && inodes.get(&inode) == Some(&port)
        {
            return true;
        }
    }
    false
}

#[cfg(all(unix, not(target_os = "linux")))]
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

#[cfg(all(unix, not(target_os = "linux")))]
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn proc_net_tcp_parser_filters_listen_uid_and_loopback() {
        let sample = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:2254 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1001 1 0000000000000000 100 0 0 10 0
   1: 00000000:2255 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1002 1 0000000000000000 100 0 0 10 0
   2: 0100007F:2256 00000000:0000 01 00000000:00000000 00:00000000 00000000  1000        0 1003 1 0000000000000000 100 0 0 10 0
   3: 0100007F:2257 00000000:0000 0A 00000000:00000000 00:00000000 00000000  2000        0 1004 1 0000000000000000 100 0 0 10 0
   4: 0200A8C0:2258 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1005 1 0000000000000000 100 0 0 10 0
";
        let found = parse_proc_net_listeners(sample, false, Some(1000), true);
        assert_eq!(found.get(&1001), Some(&0x2254));
        assert_eq!(found.get(&1002), Some(&0x2255));
        assert!(!found.contains_key(&1003));
        assert!(!found.contains_key(&1004));
        assert!(!found.contains_key(&1005));
        assert_eq!(parse_socket_inode(Path::new("socket:[1001]")), Some(1001));
        assert_eq!(parse_socket_inode(Path::new("pipe:[1001]")), None);
    }
}

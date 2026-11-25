//! SSH bootstrap and connection handling (port of `packages/env/src/ssh.ts`).
//!
//! Host keys are checked against the application's own known-hosts file; a key is never accepted automatically.
//! SSH helpers shell out to the system `ssh` and `ssh-keygen`, exactly as the TypeScript implementation does.

use crate::connection::{CommandSource, Connection, ConnectionOptions, LogCallback, RemoteError};
use crate::sha256::sha256_hex;
use base64::Engine;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

const SSH_TIMEOUT: Duration = Duration::from_secs(60);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An `ssh` invocation that failed for another reason, with its exit code and diagnostics.
#[derive(Debug)]
pub struct SshError {
    pub message: String,
    pub exit_code: Option<i32>,
    pub stderr: String,
}

impl fmt::Display for SshError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SshError {}

/// The remote host's key is not in the known-hosts file; `scan_host_key` shows it so the owner can accept it.
#[derive(Debug, Clone)]
pub struct HostKeyUnknownError {
    pub host: String,
}

impl fmt::Display for HostKeyUnknownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "The host key of {} is not trusted yet",
            self.host
        )
    }
}

impl std::error::Error for HostKeyUnknownError {}

/// The remote host's key differs from the one already trusted; it is never accepted automatically.
#[derive(Debug, Clone)]
pub struct HostKeyChangedError {
    pub host: String,
}

impl fmt::Display for HostKeyChangedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "The host key of {} changed; remove the old key to continue",
            self.host
        )
    }
}

impl std::error::Error for HostKeyChangedError {}

#[derive(Debug)]
pub enum SshFailure {
    Ssh(SshError),
    UnknownKey(HostKeyUnknownError),
    ChangedKey(HostKeyChangedError),
    Invalid(String),
    Io(std::io::Error),
}

impl fmt::Display for SshFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SshFailure::Ssh(error) => write!(formatter, "{error}"),
            SshFailure::UnknownKey(error) => write!(formatter, "{error}"),
            SshFailure::ChangedKey(error) => write!(formatter, "{error}"),
            SshFailure::Invalid(message) => write!(formatter, "{message}"),
            SshFailure::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for SshFailure {}

impl From<std::io::Error> for SshFailure {
    fn from(error: std::io::Error) -> SshFailure {
        SshFailure::Io(error)
    }
}

impl From<SshError> for SshFailure {
    fn from(error: SshError) -> SshFailure {
        SshFailure::Ssh(error)
    }
}

impl From<HostKeyUnknownError> for SshFailure {
    fn from(error: HostKeyUnknownError) -> SshFailure {
        SshFailure::UnknownKey(error)
    }
}

impl From<HostKeyChangedError> for SshFailure {
    fn from(error: HostKeyChangedError) -> SshFailure {
        SshFailure::ChangedKey(error)
    }
}

impl SshFailure {
    /// A failed daemon start fails the requests waiting for it with a spawn error and the ssh diagnostics as message.
    fn into_remote_error(self) -> RemoteError {
        RemoteError::new(&serde_json::json!({
            "code": "spawn_error",
            "message": self.to_string(),
            "lost": true,
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemotePlatformKind {
    Linux,
    Android,
    Darwin,
    Windows,
}

impl RemotePlatformKind {
    fn as_str(self) -> &'static str {
        match self {
            RemotePlatformKind::Linux => "linux",
            RemotePlatformKind::Android => "android",
            RemotePlatformKind::Darwin => "darwin",
            RemotePlatformKind::Windows => "windows",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteArch {
    X64,
    Arm64,
}

impl RemoteArch {
    fn as_str(self) -> &'static str {
        match self {
            RemoteArch::X64 => "x64",
            RemoteArch::Arm64 => "arm64",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteShell {
    Cmd,
    Powershell,
}

/// A remote system the package ships a daemon for.
#[derive(Debug, Clone)]
pub struct RemotePlatform {
    pub platform: RemotePlatformKind,
    pub arch: RemoteArch,
    pub home: String,
    pub shell: Option<RemoteShell>,
    pub warnings: Vec<String>,
}

/// How to reach a machine with the system `ssh`.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
    pub known_hosts_file: String,
    pub host_key_alias: String,
    pub ssh: Option<String>,
    pub config_file: Option<String>,
}

fn check_field(name: &str, value: &str) -> Result<(), SshFailure> {
    if value.is_empty()
        || value.starts_with('-')
        || value.chars().any(|character| {
            character.is_whitespace() || (character as u32) <= 0x1f || character == '\u{7f}'
        })
    {
        return Err(SshFailure::Invalid(format!("Invalid {name}: {value:?}")));
    }
    Ok(())
}

/// A path for an `ssh` option that expands `%` tokens: quoted, with `%` literal.
fn config_path(name: &str, path: &str) -> Result<String, SshFailure> {
    if path.is_empty()
        || path.chars().any(|character| {
            character == '"' || (character as u32) <= 0x1f || character == '\u{7f}'
        })
    {
        return Err(SshFailure::Invalid(format!("Invalid {name}: {path:?}")));
    }
    Ok(format!("\"{}\"", path.replace('%', "%%")))
}

/**
 * Arguments for `ssh` up to the host: no prompts, no forwarding of any kind, no shared connections, no commands from
 * the configuration, no locale forwarding, and host keys checked strictly against the application's own file.
 */
pub fn ssh_arguments(
    target: &SshTarget,
    strict_host_keys: bool,
    known_hosts_file: Option<&str>,
) -> Result<Vec<String>, SshFailure> {
    check_field("host", &target.host)?;
    check_field("host key alias", &target.host_key_alias)?;
    if let Some(user) = &target.user {
        check_field("user", user)?;
    }
    if let Some(port) = target.port
        && !(1..=65535).contains(&port)
    {
        return Err(SshFailure::Invalid(format!("Invalid port: {port}")));
    }
    let known_hosts = known_hosts_file.unwrap_or(&target.known_hosts_file);
    let option = |setting: &str| ["-o".to_string(), setting.to_string()];
    let mut args = Vec::new();
    if let Some(config) = &target.config_file {
        args.extend(["-F".to_string(), config.clone()]);
    }
    args.extend(["-T".into(), "-a".into(), "-x".into()]);
    for setting in [
        "BatchMode=yes",
        "ClearAllForwardings=yes",
        "ForwardAgent=no",
        "ForwardX11=no",
        "ControlMaster=no",
        "ControlPath=none",
        "RemoteCommand=none",
        "PermitLocalCommand=no",
        "SendEnv=-*",
        "ConnectTimeout=15",
        "ServerAliveInterval=15",
    ] {
        args.extend(option(setting));
    }
    args.extend(option(&format!(
        "StrictHostKeyChecking={}",
        if strict_host_keys {
            "yes"
        } else {
            "accept-new"
        }
    )));
    args.extend(option(&format!(
        "UserKnownHostsFile={}",
        config_path("known hosts file", known_hosts)?
    )));
    args.extend(option("GlobalKnownHostsFile=none"));
    args.extend(option("HashKnownHosts=no"));
    args.extend(option(&format!("HostKeyAlias={}", target.host_key_alias)));
    if let Some(user) = &target.user {
        args.extend(["-l".into(), user.clone()]);
    }
    if let Some(port) = target.port {
        args.extend(["-p".into(), port.to_string()]);
    }
    if let Some(identity) = &target.identity_file {
        args.extend(option(&format!(
            "IdentityFile={}",
            config_path("identity file", identity)?
        )));
        args.extend(option("IdentitiesOnly=yes"));
    }
    args.extend(["--".into(), target.host.clone()]);
    Ok(args)
}

fn classify_stderr(host: &str, stderr: &str) -> Option<SshFailure> {
    if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return Some(
            HostKeyChangedError {
                host: host.to_string(),
            }
            .into(),
        );
    }
    if stderr.contains("Host key verification failed")
        || (stderr.contains("No ") && stderr.contains(" host key is known"))
    {
        return Some(
            HostKeyUnknownError {
                host: host.to_string(),
            }
            .into(),
        );
    }
    None
}

fn run_ssh(
    target: &SshTarget,
    command: &str,
    stdin: Vec<u8>,
    args: Option<Vec<String>>,
    timeout: Duration,
) -> Result<String, SshFailure> {
    let args = match args {
        Some(args) => args,
        None => ssh_arguments(target, true, None)?,
    };
    let mut child = Command::new(target.ssh.as_deref().unwrap_or("ssh"))
        .args(args)
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // INFALLIBLE: all three streams were configured as pipes above.
    let mut child_stdin = child.stdin.take().expect("piped ssh stdin");
    let mut stdout = child.stdout.take().expect("piped ssh stdout");
    let mut stderr = child.stderr.take().expect("piped ssh stderr");
    let (stdout_sender, stdout_receiver) = std::sync::mpsc::channel::<Vec<u8>>();
    let (stderr_sender, stderr_receiver) = std::sync::mpsc::channel::<Vec<u8>>();
    let sender = stdout_sender;
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if sender.send(chunk[..read].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let sender = stderr_sender;
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if sender.send(chunk[..read].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    std::thread::spawn(move || {
        let _ = child_stdin.write_all(&stdin);
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let output: Vec<u8> = stdout_receiver.into_iter().flatten().collect();
    let diagnostics: Vec<u8> = stderr_receiver.into_iter().flatten().collect();
    let diagnostics = String::from_utf8_lossy(&diagnostics).into_owned();
    let Some(status) = status else {
        return Err(SshError {
            message: format!(
                "ssh {} did not finish within {} s: {}",
                target.host,
                timeout.as_secs(),
                diagnostics.trim()
            ),
            exit_code: None,
            stderr: diagnostics,
        }
        .into());
    };
    if status.success() {
        return Ok(String::from_utf8_lossy(&output).into_owned());
    }
    if let Some(failure) = classify_stderr(&target.host, &diagnostics) {
        return Err(failure);
    }
    Err(SshError {
        message: format!(
            "ssh {} failed with exit code {}: {}",
            target.host,
            status.code().unwrap_or(-1),
            diagnostics.trim()
        ),
        exit_code: status.code(),
        stderr: diagnostics,
    }
    .into())
}

/// Host keys scanned with `accept-new`, plus their fingerprints from `ssh-keygen -lf`.
#[derive(Debug)]
pub struct ScannedHostKey {
    pub lines: Vec<String>,
    pub fingerprints: Vec<String>,
}

/// Connect once with `accept-new` against a temporary known-hosts file to capture the key the host presents.
pub fn scan_host_key(target: &SshTarget) -> Result<ScannedHostKey, SshFailure> {
    let directory = temp_dir("pi-env-hostkey-")?;
    let result = (|| {
        let scanned = directory.join("known_hosts");
        let args = ssh_arguments(target, false, Some(&scanned.to_string_lossy()))?;
        match run_ssh(target, "exit 0", Vec::new(), Some(args), SSH_TIMEOUT) {
            Ok(_) | Err(SshFailure::Ssh(_)) | Err(SshFailure::UnknownKey(_)) => {}
            Err(error) => return Err(error),
        }
        let lines = if scanned.exists() {
            fs::read_to_string(&scanned)?
                .split('\n')
                .filter(|line| !line.trim().is_empty())
                .map(str::to_string)
                .collect()
        } else {
            Vec::new()
        };
        if lines.is_empty() {
            return Err(SshFailure::Invalid(format!(
                "No host key received from {}",
                target.host
            )));
        }
        let output = Command::new("ssh-keygen")
            .arg("-lf")
            .arg(&scanned)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()?;
        let fingerprints = String::from_utf8_lossy(&output.stdout)
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect();
        Ok(ScannedHostKey {
            lines,
            fingerprints,
        })
    })();
    let _ = fs::remove_dir_all(&directory);
    result
}

fn temp_dir(prefix: &str) -> Result<PathBuf, SshFailure> {
    let path =
        std::env::temp_dir().join(format!("{prefix}{}-{}", std::process::id(), random_hex(8)));
    fs::create_dir(&path)?;
    Ok(path)
}

fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A known-hosts line for `alias`: `alias type key [comment]`, no markers, patterns or hashed names.
fn parse_host_key_line(alias: &str, line: &str) -> Result<(String, String), SshFailure> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let invalid = || SshFailure::Invalid(format!("Not a host key line for {alias}: {line:?}"));
    let (host, key_type, key) = match (fields.first(), fields.get(1), fields.get(2)) {
        (Some(host), Some(key_type), Some(key)) => (*host, *key_type, *key),
        _ => return Err(invalid()),
    };
    if host != alias || !valid_key_type(key_type) || !valid_key(key) {
        return Err(invalid());
    }
    Ok((key_type.to_string(), key.to_string()))
}

fn valid_key_type(key_type: &str) -> bool {
    if let Some(rest) = key_type
        .strip_prefix("ssh-")
        .or_else(|| key_type.strip_prefix("ecdsa-sha2-"))
    {
        return !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    }
    if let Some(rest) = key_type.strip_prefix("sk-") {
        return !rest.is_empty()
            && rest.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '@' | '.')
            });
    }
    false
}

fn valid_key(key: &str) -> bool {
    let body = key.trim_end_matches('=');
    let padding = key.len() - body.len();
    !body.is_empty()
        && padding <= 2
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/'))
}

static KNOWN_HOSTS_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

fn known_hosts_lock(file: &Path) -> Arc<Mutex<()>> {
    let locks = KNOWN_HOSTS_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = lock(locks);
    Arc::clone(
        locks
            .entry(file.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

/// Rewrite the known-hosts file one change at a time, atomically.
fn change_known_hosts(
    file: &Path,
    change: impl FnOnce(&[String]) -> Result<Vec<String>, SshFailure>,
) -> Result<(), SshFailure> {
    let serial = known_hosts_lock(file);
    let _serial = lock(&serial);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }
    let existing: Vec<String> = if file.exists() {
        fs::read_to_string(file)?
            .split('\n')
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };
    let lines = change(&existing)?;
    let temporary = file.with_extension(format!("{}.tmp", random_hex(6)));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options
        .open(&temporary)
        .and_then(|mut handle| {
            let content = if lines.is_empty() {
                String::new()
            } else {
                format!("{}\n", lines.join("\n"))
            };
            handle.write_all(content.as_bytes())
        })
        .map_err(SshFailure::from);
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, file) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

/// Trust host-key lines from `scan_host_key` by adding them to the target's known-hosts file.
pub fn accept_host_key(target: &SshTarget, lines: &[String]) -> Result<(), SshFailure> {
    let alias = target.host_key_alias.clone();
    let mut accepted = Vec::new();
    for line in lines {
        let trimmed = line.trim().to_string();
        let (key_type, key) = parse_host_key_line(&alias, line)?;
        accepted.push((trimmed, key_type, key));
    }
    let host = target.host.clone();
    change_known_hosts(Path::new(&target.known_hosts_file), |existing| {
        let mut trusted: HashMap<String, String> = HashMap::new();
        for line in existing {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if let (Some(&stored_alias), Some(key_type), Some(key)) =
                (fields.first(), fields.get(1), fields.get(2))
                && stored_alias == alias
            {
                trusted.insert((*key_type).to_string(), (*key).to_string());
            }
        }
        let mut added = Vec::new();
        for (line, key_type, key) in accepted {
            match trusted.get(&key_type) {
                Some(known) if known == &key => {}
                Some(_) => {
                    return Err(HostKeyChangedError { host: host.clone() }.into());
                }
                None => {
                    trusted.insert(key_type, key);
                    added.push(line);
                }
            }
        }
        let mut result = existing.to_vec();
        result.extend(added);
        Ok(result)
    })
}

/// Stop trusting every key stored for the target's alias.
pub fn forget_host_key(target: &SshTarget) -> Result<(), SshFailure> {
    let alias = target.host_key_alias.clone();
    change_known_hosts(Path::new(&target.known_hosts_file), |existing| {
        Ok(existing
            .iter()
            .filter(|line| line.split_whitespace().next() != Some(alias.as_str()))
            .cloned()
            .collect())
    })
}

const POSIX_PROBE: &str = r#"sh -c 'echo PI-ENV-PROBE; uname -s; uname -m; uname -o 2>/dev/null || echo -; printf "%s\n" "$HOME" "${TMPDIR:--}" "${LD_PRELOAD:--}"'"#;

fn powershell(script: &str) -> String {
    let mut bytes = Vec::new();
    for unit in script.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    format!(
        "powershell -NoProfile -NonInteractive -EncodedCommand {}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

fn windows_probe() -> String {
    powershell(
        "'PI-ENV-PROBE'; 'Windows'; [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString(); '-'; $HOME; '-'; '-'",
    )
}

fn normalize_arch(machine: &str) -> Result<RemoteArch, SshFailure> {
    match machine.to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => Ok(RemoteArch::X64),
        "aarch64" | "arm64" => Ok(RemoteArch::Arm64),
        other => Err(SshFailure::Invalid(format!(
            "Unsupported remote architecture: {other}"
        ))),
    }
}

#[derive(Debug)]
struct Probe {
    system: String,
    machine: String,
    os: String,
    home: String,
    tmpdir: String,
    preload: String,
}

fn parse_probe(target: &SshTarget, output: &str) -> Result<Probe, SshFailure> {
    let lines: Vec<&str> = output
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    let start = lines
        .iter()
        .position(|line| *line == "PI-ENV-PROBE")
        .ok_or_else(|| {
            SshFailure::Invalid(format!(
                "Unexpected answer from {}: {}",
                target.host,
                output.trim()
            ))
        })?;
    let field = |index: usize, default: &str| {
        lines
            .get(start + 1 + index)
            .map(|line| (*line).to_string())
            .unwrap_or_else(|| default.to_string())
    };
    Ok(Probe {
        system: field(0, ""),
        machine: field(1, ""),
        os: field(2, ""),
        home: field(3, ""),
        tmpdir: field(4, "-"),
        preload: field(5, "-"),
    })
}

fn termux_warnings(probe: &Probe) -> Vec<String> {
    let mut warnings = Vec::new();
    if probe.tmpdir == "-" {
        warnings
            .push("TMPDIR is not set; Termux's sshd normally sets it to $PREFIX/tmp.".to_string());
    }
    if !probe.preload.contains("termux-exec") {
        warnings.push(
            "termux-exec is not loaded (LD_PRELOAD); scripts with #!/usr/bin/env shebangs will fail.".to_string(),
        );
    }
    warnings.push(
        "Android may suspend Termux; run termux-wake-lock on the device to keep the connection alive.".to_string(),
    );
    warnings
}

/// Which system the target runs: `uname` through the login shell, or PowerShell on Windows.
pub fn detect_platform(target: &SshTarget) -> Result<RemotePlatform, SshFailure> {
    let mut probe = match run_ssh(target, POSIX_PROBE, Vec::new(), None, SSH_TIMEOUT) {
        Ok(output) => parse_probe(target, &output)?,
        Err(error @ (SshFailure::UnknownKey(_) | SshFailure::ChangedKey(_))) => return Err(error),
        Err(_) => parse_probe(
            target,
            &run_ssh(target, &windows_probe(), Vec::new(), None, SSH_TIMEOUT)?,
        )?,
    };
    if probe.system.starts_with("MINGW")
        || probe.system.starts_with("MSYS")
        || probe.system.starts_with("CYGWIN")
    {
        probe = parse_probe(
            target,
            &run_ssh(target, &windows_probe(), Vec::new(), None, SSH_TIMEOUT)?,
        )?;
    }
    let arch = normalize_arch(&probe.machine)?;
    match probe.system.as_str() {
        "Windows" => {
            let shell = if run_ssh(target, "echo %OS%", Vec::new(), None, SSH_TIMEOUT)?
                .contains("Windows_NT")
            {
                RemoteShell::Cmd
            } else {
                RemoteShell::Powershell
            };
            Ok(RemotePlatform {
                platform: RemotePlatformKind::Windows,
                arch,
                home: probe.home,
                shell: Some(shell),
                warnings: Vec::new(),
            })
        }
        "Darwin" => Ok(RemotePlatform {
            platform: RemotePlatformKind::Darwin,
            arch,
            home: probe.home,
            shell: None,
            warnings: Vec::new(),
        }),
        "Linux" => {
            let android = probe.os == "Android";
            Ok(RemotePlatform {
                platform: if android {
                    RemotePlatformKind::Android
                } else {
                    RemotePlatformKind::Linux
                },
                arch,
                home: probe.home.clone(),
                shell: None,
                warnings: if android {
                    termux_warnings(&probe)
                } else {
                    Vec::new()
                },
            })
        }
        other => Err(SshFailure::Invalid(format!(
            "Unsupported remote system: {other}"
        ))),
    }
}

const DAEMON_DIR_ENV: &str = "PI_ENV_DAEMON_DIR";

fn host_platform() -> Option<RemotePlatformKind> {
    match std::env::consts::OS {
        "linux" => Some(RemotePlatformKind::Linux),
        "android" => Some(RemotePlatformKind::Android),
        "macos" => Some(RemotePlatformKind::Darwin),
        "windows" => Some(RemotePlatformKind::Windows),
        _ => None,
    }
}

fn host_arch() -> Option<RemoteArch> {
    match std::env::consts::ARCH {
        "x86_64" => Some(RemoteArch::X64),
        "aarch64" => Some(RemoteArch::Arm64),
        _ => None,
    }
}

fn push_daemon_candidates(
    candidates: &mut Vec<PathBuf>,
    dir: &Path,
    target_name: &str,
    flat_target_name: &str,
    binary_name: &str,
    matches_host: bool,
) {
    candidates.push(dir.join(target_name).join(binary_name));
    candidates.push(dir.join(flat_target_name));
    if matches_host {
        candidates.push(dir.join(binary_name));
    }
}

/// Pure, testable daemon path resolver for `packaged_daemon`.
///
/// Checks in order:
/// 1. `env_daemon_dir` (`PI_ENV_DAEMON_DIR`), supporting `<dir>/pi-env-<platform>-<arch>/pi-env[.exe]`,
///    `<dir>/pi-env-<platform>-<arch>[.exe]`, and `<dir>/pi-env[.exe]` when `remote` matches the host.
/// 2. Native Cargo / release layout beside `exe_path`: `<exe_dir>/pi-env-<platform>-<arch>/pi-env[.exe]`,
///    `<exe_dir>/pi-env-<platform>-<arch>[.exe]`, and `<exe_dir>/pi-env[.exe]` when `remote` matches the host.
/// 3. Legacy npm `bin/` layout: `<exe_dir>/../bin/pi-env-<platform>-<arch>/pi-env[.exe]`.
///
/// Returns the first existing file, or the preferred native/override path if none exist.
pub fn packaged_daemon_in(
    exe_path: Option<&Path>,
    env_daemon_dir: Option<&Path>,
    host_platform: Option<RemotePlatformKind>,
    host_arch: Option<RemoteArch>,
    remote: &RemotePlatform,
) -> PathBuf {
    let is_windows = remote.platform == RemotePlatformKind::Windows;
    let binary_name = if is_windows { "pi-env.exe" } else { "pi-env" };
    let target_name = format!(
        "pi-env-{}-{}",
        remote.platform.as_str(),
        remote.arch.as_str()
    );
    let flat_target_name = if is_windows {
        format!("{target_name}.exe")
    } else {
        target_name.clone()
    };
    let matches_host = host_platform == Some(remote.platform) && host_arch == Some(remote.arch);

    let mut candidates = Vec::new();
    if let Some(dir) = env_daemon_dir.filter(|path| !path.as_os_str().is_empty()) {
        push_daemon_candidates(
            &mut candidates,
            dir,
            &target_name,
            &flat_target_name,
            binary_name,
            matches_host,
        );
    }
    if let Some(exe_dir) = exe_path
        .and_then(Path::parent)
        .filter(|path| !path.as_os_str().is_empty())
    {
        push_daemon_candidates(
            &mut candidates,
            exe_dir,
            &target_name,
            &flat_target_name,
            binary_name,
            matches_host,
        );
        if let Some(parent_dir) = exe_dir.parent() {
            candidates.push(parent_dir.join("bin").join(&target_name).join(binary_name));
        }
    }

    if let Some(found) = candidates.iter().find(|candidate| candidate.is_file()) {
        return found.clone();
    }
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| PathBuf::from(&target_name).join(binary_name))
}

/// The daemon binary shipped for `remote`, checking `PI_ENV_DAEMON_DIR`, native layout beside the host binary, and legacy `../bin/`.
pub fn packaged_daemon(remote: &RemotePlatform) -> PathBuf {
    let exe_path = std::env::current_exe().ok();
    let env_dir = std::env::var_os(DAEMON_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    packaged_daemon_in(
        exe_path.as_deref(),
        env_dir.as_deref(),
        host_platform(),
        host_arch(),
        remote,
    )
}

/// The daemon this package ships for the machine it runs on, e.g. for terminals of local sessions.
pub fn local_daemon() -> Result<PathBuf, SshFailure> {
    let local = std::env::current_exe()?;
    let bin_name = if cfg!(windows) {
        "pi-env.exe"
    } else {
        "pi-env"
    };
    // Walk up a bounded number of ancestor directories so `local_daemon` works
    // beside the executable, under Cargo's `target/<profile>/deps/`, and under
    // Cargo 1.100+'s `target/<profile>/build/<crate>/<hash>/out/`.
    for ancestor in local.ancestors().skip(1).take(6) {
        let candidate = ancestor.join(bin_name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(SshFailure::Invalid(
        "No pi-env daemon found next to this binary".to_string(),
    ))
}

fn quote_posix(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn quote_powershell(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Where a daemon with this content lives on the remote machine: named by its SHA-256, so versions never collide.
fn daemon_path(remote: &RemotePlatform, sha256: &str) -> String {
    let name = format!("pi-env-{}", &sha256[..32]);
    if remote.platform == RemotePlatformKind::Windows {
        format!("{}\\.pi\\mobile\\tools\\{name}.exe", remote.home)
    } else {
        format!("{}/.pi/mobile/tools/{name}", remote.home)
    }
}

const POSIX_HASH: &str = "hash() { if command -v sha256sum >/dev/null 2>&1; then sha256sum \"$1\" | cut -d' ' -f1; elif command -v shasum >/dev/null 2>&1; then shasum -a 256 \"$1\" | cut -d' ' -f1; elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 \"$1\" | sed 's/.*= //'; else echo none; fi; }";

fn check_daemon(
    target: &SshTarget,
    remote: &RemotePlatform,
    file: &str,
    sha256: &str,
) -> Result<String, SshFailure> {
    if remote.platform == RemotePlatformKind::Windows {
        let check = powershell(&format!(
            "$f = {}; if ((Test-Path -LiteralPath $f) -and ((Get-FileHash -Algorithm SHA256 -LiteralPath $f).Hash.ToLower() -eq '{}')) {{ 'present' }} else {{ 'missing' }}",
            quote_powershell(file),
            sha256
        ));
        return Ok(if run_ssh(target, &check, Vec::new(), None, SSH_TIMEOUT)?
            .trim()
            .ends_with("present")
        {
            "present"
        } else {
            "missing"
        }
        .to_string());
    }
    let check = format!(
        "{}; f={}; if [ -f \"$f\" ] && [ \"$(hash \"$f\")\" = {} ]; then echo present; elif [ \"$(hash /dev/null)\" = none ]; then echo nohash; else echo missing; fi",
        POSIX_HASH,
        quote_posix(file),
        sha256
    );
    let output = run_ssh(
        target,
        &format!("sh -c {}", quote_posix(&check)),
        Vec::new(),
        None,
        SSH_TIMEOUT,
    )?;
    Ok(output
        .trim()
        .split('\n')
        .next_back()
        .unwrap_or("missing")
        .to_string())
}

fn base64_lines(bytes: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let mut lines = String::new();
    for (index, character) in encoded.chars().enumerate() {
        if index > 0 && index % 76 == 0 {
            lines.push('\n');
        }
        lines.push(character);
    }
    lines.push_str("PI-ENV-END\n");
    lines
}

/// Make sure the daemon is on the remote machine, verified by its SHA-256 before it ever runs.
pub fn deploy_daemon(
    target: &SshTarget,
    remote: &RemotePlatform,
    binary: Option<&Path>,
) -> Result<String, SshFailure> {
    let binary = match binary {
        Some(path) => path.to_path_buf(),
        None => packaged_daemon(remote),
    };
    if !binary.is_file() {
        return Err(SshFailure::Invalid(format!(
            "No pi-env daemon for {}-{} at {}",
            remote.platform.as_str(),
            remote.arch.as_str(),
            binary.display()
        )));
    }
    let bytes = fs::read(&binary)?;
    let sha256 = sha256_hex(&bytes);
    let file = daemon_path(remote, &sha256);
    match check_daemon(target, remote, &file, &sha256)?.as_str() {
        "present" => return Ok(file),
        "nohash" => {
            return Err(SshFailure::Invalid(format!(
                "{} has no sha256sum, shasum or openssl to verify pi-env",
                target.host
            )));
        }
        _ => {}
    }
    if remote.platform == RemotePlatformKind::Windows {
        let script = [
            "$ErrorActionPreference = 'Stop'",
            &format!("$f = {}", quote_powershell(&file)),
            "$d = Split-Path -Parent $f",
            "New-Item -ItemType Directory -Force -Path $d | Out-Null",
            "$t = Join-Path $d ('.pi-env-' + [guid]::NewGuid().ToString() + '.tmp')",
            "$text = New-Object System.Text.StringBuilder",
            "foreach ($line in $input) { if ($line -eq 'PI-ENV-END') { break }; [void]$text.Append($line) }",
            "$bytes = [Convert]::FromBase64String($text.ToString())",
            "$out = [IO.File]::Open($t, 'CreateNew', 'Write', 'None'); $out.Write($bytes, 0, $bytes.Length); $out.Close()",
            &format!("if ((Get-FileHash -Algorithm SHA256 -LiteralPath $t).Hash.ToLower() -ne '{}') {{ Remove-Item -LiteralPath $t; throw 'pi-env upload is corrupt' }}", sha256),
            "for ($i = 0; ; $i++) { try { Move-Item -Force -LiteralPath $t -Destination $f; break } catch { if ($i -ge 20) { throw }; Start-Sleep -Milliseconds 250 } }",
            "Get-ChildItem -LiteralPath $d -Filter 'pi-env-*.exe' | Where-Object { $_.FullName -ne $f } | ForEach-Object { Remove-Item -LiteralPath $_.FullName -ErrorAction SilentlyContinue }",
            "'deployed'",
        ]
        .join("; ");
        run_ssh(
            target,
            &powershell(&script),
            base64_lines(&bytes).into_bytes(),
            None,
            UPLOAD_TIMEOUT,
        )?;
        return Ok(file);
    }
    let upload = [
        "set -e",
        POSIX_HASH,
        &format!("f={}", quote_posix(&file)),
        "d=$(dirname \"$f\")",
        "mkdir -p \"$d\"",
        "chmod 700 \"$d\"",
        "t=$(mktemp \"$d/.pi-env.XXXXXX\")",
        "cat > \"$t\"",
        &format!("if [ \"$(hash \"$t\")\" != {} ]; then rm -f \"$t\"; echo \"pi-env upload is corrupt\" >&2; exit 1; fi", sha256),
        "chmod 700 \"$t\"",
        "mv -f \"$t\" \"$f\"",
        "for old in \"$d\"/pi-env-*; do [ \"$old\" = \"$f\" ] || rm -f \"$old\"; done",
        "echo deployed",
    ]
    .join("\n");
    run_ssh(
        target,
        &format!("sh -c {}", quote_posix(&upload)),
        bytes,
        None,
        UPLOAD_TIMEOUT,
    )?;
    Ok(file)
}

/// Options for `ssh_connection` and `connect_ssh`: the target, plus the binary to deploy.
#[derive(Clone)]
pub struct SshConnectOptions {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
    pub known_hosts_file: String,
    pub host_key_alias: String,
    pub ssh: Option<String>,
    pub config_file: Option<String>,
    pub binary: Option<PathBuf>,
    pub login_shell: bool,
    pub on_log: Option<LogCallback>,
}

impl SshConnectOptions {
    fn target(&self) -> SshTarget {
        SshTarget {
            host: self.host.clone(),
            user: self.user.clone(),
            port: self.port,
            identity_file: self.identity_file.clone(),
            known_hosts_file: self.known_hosts_file.clone(),
            host_key_alias: self.host_key_alias.clone(),
            ssh: self.ssh.clone(),
            config_file: self.config_file.clone(),
        }
    }
}

/// The remote command that starts the daemon at `file`, before the `serve` arguments.
fn launch_command(remote: &RemotePlatform, file: &str, login_shell: bool) -> String {
    if remote.platform == RemotePlatformKind::Windows {
        if remote.shell == Some(RemoteShell::Powershell) {
            return format!("& {}", quote_powershell(file));
        }
        return if file.contains(' ') {
            format!("\"{file}\"")
        } else {
            file.to_string()
        };
    }
    if login_shell {
        format!(
            "exec \"$SHELL\" -lc 'exec \"$0\" \"$@\"' {}",
            quote_posix(file)
        )
    } else {
        quote_posix(file)
    }
}

#[derive(Default)]
struct SshState {
    remote: Option<RemotePlatform>,
    verified: Option<String>,
}

/// A `Connection` whose every start detects the remote system (once), verifies or deploys the daemon, then starts it.
pub struct SshConnection {
    pub connection: Connection,
    state: Arc<Mutex<SshState>>,
}

impl SshConnection {
    /// The system detected so far, once known.
    pub fn remote(&self) -> Option<RemotePlatform> {
        lock(&self.state).remote.clone()
    }
}

fn ssh_connection_for(options: SshConnectOptions, state: Arc<Mutex<SshState>>) -> Connection {
    let on_log = options.on_log.clone();
    let command_options = options.clone();
    let command_state = Arc::clone(&state);
    let command = CommandSource::Dynamic(Arc::new(move || {
        let target = command_options.target();
        let mut state = lock(&command_state);
        if state.remote.is_none() {
            let remote = detect_platform(&target).map_err(SshFailure::into_remote_error)?;
            for warning in &remote.warnings {
                if let Some(callback) = &command_options.on_log {
                    callback(&format!("{warning}\n"));
                }
            }
            state.remote = Some(remote);
        }
        // INFALLIBLE: detection succeeded immediately above, or the function already returned its error.
        let remote = state.remote.clone().expect("remote was just detected");
        let file = match state.verified.take() {
            Some(file) => file,
            None => deploy_daemon(&target, &remote, command_options.binary.as_deref())
                .map_err(SshFailure::into_remote_error)?,
        };
        let mut command = vec![
            command_options
                .ssh
                .clone()
                .unwrap_or_else(|| "ssh".to_string()),
        ];
        command.extend(ssh_arguments(&target, true, None).map_err(SshFailure::into_remote_error)?);
        command.push(launch_command(&remote, &file, command_options.login_shell));
        Ok(command)
    }));
    Connection::new(ConnectionOptions { command, on_log })
}

/// A `Connection` to the target that does nothing until its first request.
pub fn ssh_connection(options: SshConnectOptions) -> SshConnection {
    let state = Arc::new(Mutex::new(SshState::default()));
    SshConnection {
        connection: ssh_connection_for(options, Arc::clone(&state)),
        state,
    }
}

/// Detect the remote system and deploy the daemon now, then return a `Connection` that starts it over `ssh`.
pub fn connect_ssh(options: SshConnectOptions) -> Result<(Connection, RemotePlatform), SshFailure> {
    let target = options.target();
    let remote = detect_platform(&target)?;
    let verified = deploy_daemon(&target, &remote, options.binary.as_deref())?;
    for warning in &remote.warnings {
        if let Some(callback) = &options.on_log {
            callback(&format!("{warning}\n"));
        }
    }
    let state = Arc::new(Mutex::new(SshState {
        remote: Some(remote.clone()),
        verified: Some(verified),
    }));
    Ok((ssh_connection_for(options, state), remote))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> SshTarget {
        SshTarget {
            host: "example.test".into(),
            user: Some("pi".into()),
            port: Some(2222),
            identity_file: Some("/keys/id_ed25519".into()),
            known_hosts_file: "/app/known hosts".into(),
            host_key_alias: "pi-env-test".into(),
            ssh: None,
            config_file: None,
        }
    }

    #[test]
    fn builds_the_hardened_ssh_argument_list() {
        let args = ssh_arguments(&target(), true, None).unwrap();
        assert_eq!(&args[..5], &["-T", "-a", "-x", "-o", "BatchMode=yes"]);
        assert!(args.contains(&"StrictHostKeyChecking=yes".to_string()));
        assert!(args.contains(&"UserKnownHostsFile=\"/app/known hosts\"".to_string()));
        assert!(args.contains(&"HostKeyAlias=pi-env-test".to_string()));
        assert!(args.contains(&"IdentityFile=\"/keys/id_ed25519\"".to_string()));
        assert!(args.contains(&"IdentitiesOnly=yes".to_string()));
        assert!(args.contains(&"-l".to_string()));
        assert!(args.contains(&"pi".to_string()));
        assert!(args.contains(&"2222".to_string()));
        assert_eq!(args.last().unwrap(), "example.test");
        assert_eq!(args.iter().filter(|arg| *arg == "-o").count(), 18);
    }

    #[test]
    fn accept_new_and_config_paths_escape_percent_signs() {
        let args = ssh_arguments(&target(), false, Some("/tmp/%h/known")).unwrap();
        assert!(args.contains(&"StrictHostKeyChecking=accept-new".to_string()));
        assert!(args.contains(&"UserKnownHostsFile=\"/tmp/%%h/known\"".to_string()));
    }

    #[test]
    fn rejects_unsafe_fields() {
        let mut unsafe_target = target();
        unsafe_target.host = "-oProxyCommand=evil".into();
        assert!(matches!(
            ssh_arguments(&unsafe_target, true, None),
            Err(SshFailure::Invalid(_))
        ));
        unsafe_target = target();
        unsafe_target.host_key_alias = "with space".into();
        assert!(matches!(
            ssh_arguments(&unsafe_target, true, None),
            Err(SshFailure::Invalid(_))
        ));
        unsafe_target = target();
        unsafe_target.port = Some(0);
        assert!(matches!(
            ssh_arguments(&unsafe_target, true, None),
            Err(SshFailure::Invalid(_))
        ));
    }

    #[test]
    fn parses_only_plain_key_lines_for_the_alias() {
        assert_eq!(
            parse_host_key_line("alias", "alias ssh-ed25519 AAAA+/x== comment").unwrap(),
            ("ssh-ed25519".to_string(), "AAAA+/x==".to_string())
        );
        assert!(parse_host_key_line("alias", "other ssh-ed25519 AAAA").is_err());
        assert!(parse_host_key_line("alias", "@cert-authority alias ssh-ed25519 AAAA").is_err());
        assert!(parse_host_key_line("alias", "alias bad-type AAAA").is_err());
        assert!(parse_host_key_line("alias", "alias ssh-ed25519 AAAA===").is_err());
        assert_eq!(
            parse_host_key_line("alias", "alias sk-ssh-ed25519@openssh.com AAAA")
                .unwrap()
                .0,
            "sk-ssh-ed25519@openssh.com"
        );
    }

    #[test]
    fn accepts_keys_once_and_refuses_a_changed_one_without_forgetting() {
        let directory = temp_dir("pi-env-hostkey-test-").unwrap();
        let file = directory.join("known_hosts");
        let path = file.to_string_lossy().into_owned();
        let mut scoped = target();
        scoped.known_hosts_file = path.clone();
        let key = (1..=3).map(|_| "a").collect::<String>() + "AAA";
        let first = format!("pi-env-test ssh-ed25519 {key}");
        let second = format!("pi-env-test ssh-ed25519 {}", "b".repeat(6));
        assert!(accept_host_key(&scoped, &[format!("other ssh-ed25519 {key}")]).is_err());
        accept_host_key(&scoped, std::slice::from_ref(&first)).unwrap();
        accept_host_key(&scoped, std::slice::from_ref(&first)).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), format!("{first}\n"));
        assert!(matches!(
            accept_host_key(&scoped, std::slice::from_ref(&second)),
            Err(SshFailure::ChangedKey(_))
        ));
        forget_host_key(&scoped).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "");
        accept_host_key(&scoped, std::slice::from_ref(&second)).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), format!("{second}\n"));
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn parses_probe_output_after_a_login_banner() {
        let probe = parse_probe(
            &target(),
            "Welcome!\r\nPI-ENV-PROBE\r\nLinux\r\nx86_64\r\nGNU/Linux\r\n/home/pi\r\n-\r\n-\r\n",
        )
        .unwrap();
        assert_eq!(probe.system, "Linux");
        assert_eq!(probe.machine, "x86_64");
        assert_eq!(normalize_arch(&probe.machine).unwrap(), RemoteArch::X64);
        assert_eq!(normalize_arch("arm64").unwrap(), RemoteArch::Arm64);
        assert!(parse_probe(&target(), "no probe here").is_err());
    }

    #[test]
    fn encodes_powershell_scripts_as_utf16_base64() {
        let command = powershell("'hi'");
        assert!(command.starts_with("powershell -NoProfile -NonInteractive -EncodedCommand "));
        let encoded = command.rsplit(' ').next().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        assert_eq!(String::from_utf16_lossy(&units), "'hi'");
    }

    #[test]
    fn names_daemon_files_by_content_and_quotes_shell_words() {
        let mut remote = RemotePlatform {
            platform: RemotePlatformKind::Linux,
            arch: RemoteArch::X64,
            home: "/home/pi".into(),
            shell: None,
            warnings: Vec::new(),
        };
        assert_eq!(
            daemon_path(&remote, &"a".repeat(64)),
            format!("/home/pi/.pi/mobile/tools/pi-env-{}", "a".repeat(32))
        );
        remote.platform = RemotePlatformKind::Windows;
        assert!(daemon_path(&remote, &"a".repeat(64)).ends_with(".exe"));
        assert_eq!(quote_posix("it's"), "'it'\\''s'");
        assert_eq!(quote_powershell("it's"), "'it''s'");
        assert!(
            launch_command(&remote, "C:\\a b\\pi-env.exe", false)
                .contains("\"C:\\a b\\pi-env.exe\"")
        );
    }

    fn remote(platform: RemotePlatformKind, arch: RemoteArch) -> RemotePlatform {
        RemotePlatform {
            platform,
            arch,
            home: "/home/pi".into(),
            shell: None,
            warnings: Vec::new(),
        }
    }

    fn touch_file(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"daemon").unwrap();
    }

    #[test]
    fn packaged_daemon_prefers_env_override_over_native_and_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let override_dir = dir.path().join("override");
        let exe_path = dir.path().join("dist").join("p2");
        let exe_dir = exe_path.parent().unwrap();
        let legacy_bin = dir
            .path()
            .join("bin")
            .join("pi-env-linux-x64")
            .join("pi-env");

        touch_file(&legacy_bin);
        touch_file(&exe_dir.join("pi-env"));
        touch_file(&exe_dir.join("pi-env-linux-x64").join("pi-env"));
        touch_file(&override_dir.join("pi-env"));
        touch_file(&override_dir.join("pi-env-linux-x64"));
        let override_nested = override_dir.join("pi-env-linux-x64").join("pi-env");
        // Note: on Unix a file and dir cannot share the exact same path `pi-env-linux-x64`;
        // test nested dir first in its own subdir, then flat in another subdir.
        drop(override_nested);
        let override_nested_dir = dir.path().join("override_nested");
        let nested_file = override_nested_dir.join("pi-env-linux-x64").join("pi-env");
        touch_file(&nested_file);
        touch_file(&override_nested_dir.join("pi-env"));

        let target = remote(RemotePlatformKind::Linux, RemoteArch::X64);
        assert_eq!(
            packaged_daemon_in(
                Some(&exe_path),
                Some(&override_nested_dir),
                Some(RemotePlatformKind::Linux),
                Some(RemoteArch::X64),
                &target,
            ),
            nested_file
        );

        // An empty directory `<override>/pi-env-linux-x64/` must not match as a flat binary.
        fs::remove_file(&nested_file).unwrap();
        assert_eq!(
            packaged_daemon_in(
                Some(&exe_path),
                Some(&override_nested_dir),
                Some(RemotePlatformKind::Linux),
                Some(RemoteArch::X64),
                &target,
            ),
            override_nested_dir.join("pi-env")
        );

        // Flat `<override>/pi-env-linux-x64` works when present.
        let flat_dir = dir.path().join("override_flat");
        let flat_file = flat_dir.join("pi-env-linux-x64");
        touch_file(&flat_file);
        touch_file(&flat_dir.join("pi-env"));
        assert_eq!(
            packaged_daemon_in(
                Some(&exe_path),
                Some(&flat_dir),
                Some(RemotePlatformKind::Linux),
                Some(RemoteArch::X64),
                &target,
            ),
            flat_file
        );

        // Empty env override is ignored and falls back to native layout.
        assert_eq!(
            packaged_daemon_in(
                Some(&exe_path),
                Some(Path::new("")),
                Some(RemotePlatformKind::Linux),
                Some(RemoteArch::X64),
                &target,
            ),
            exe_dir.join("pi-env-linux-x64").join("pi-env")
        );
    }

    #[test]
    fn packaged_daemon_uses_native_sibling_only_when_remote_matches_host() {
        let dir = tempfile::tempdir().unwrap();
        let exe_path = dir.path().join("release").join("p2");
        let exe_dir = exe_path.parent().unwrap();
        let sibling = exe_dir.join("pi-env");
        let sibling_exe = exe_dir.join("pi-env.exe");
        touch_file(&sibling);
        touch_file(&sibling_exe);

        assert_eq!(
            packaged_daemon_in(
                Some(&exe_path),
                None,
                Some(RemotePlatformKind::Linux),
                Some(RemoteArch::X64),
                &remote(RemotePlatformKind::Linux, RemoteArch::X64),
            ),
            sibling
        );
        assert_eq!(
            packaged_daemon_in(
                Some(&exe_path),
                None,
                Some(RemotePlatformKind::Linux),
                Some(RemoteArch::X64),
                &remote(RemotePlatformKind::Linux, RemoteArch::Arm64),
            ),
            exe_dir.join("pi-env-linux-arm64").join("pi-env")
        );
        assert_eq!(
            packaged_daemon_in(
                Some(&exe_path),
                None,
                Some(RemotePlatformKind::Linux),
                Some(RemoteArch::X64),
                &remote(RemotePlatformKind::Windows, RemoteArch::X64),
            ),
            exe_dir.join("pi-env-windows-x64").join("pi-env.exe")
        );
        assert_eq!(
            packaged_daemon_in(
                Some(&exe_path),
                None,
                Some(RemotePlatformKind::Windows),
                Some(RemoteArch::X64),
                &remote(RemotePlatformKind::Windows, RemoteArch::X64),
            ),
            sibling_exe
        );
    }

    #[test]
    fn packaged_daemon_resolves_cross_arch_native_bundles_and_flat_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let exe_path = dir.path().join("dist").join("p2");
        let exe_dir = exe_path.parent().unwrap();

        let arm64_native = exe_dir.join("pi-env-linux-arm64").join("pi-env");
        let arm64_legacy = dir
            .path()
            .join("bin")
            .join("pi-env-linux-arm64")
            .join("pi-env");
        touch_file(&arm64_native);
        touch_file(&arm64_legacy);
        assert_eq!(
            packaged_daemon_in(
                Some(&exe_path),
                None,
                Some(RemotePlatformKind::Linux),
                Some(RemoteArch::X64),
                &remote(RemotePlatformKind::Linux, RemoteArch::Arm64),
            ),
            arm64_native
        );

        let win_flat = exe_dir.join("pi-env-windows-x64.exe");
        touch_file(&win_flat);
        assert_eq!(
            packaged_daemon_in(
                Some(&exe_path),
                None,
                Some(RemotePlatformKind::Linux),
                Some(RemoteArch::X64),
                &remote(RemotePlatformKind::Windows, RemoteArch::X64),
            ),
            win_flat
        );
    }

    #[test]
    fn packaged_daemon_falls_back_to_legacy_bin_layout_and_returns_preferred_native_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let exe_path = dir.path().join("dist").join("p2");
        let exe_dir = exe_path.parent().unwrap();
        let legacy = dir
            .path()
            .join("bin")
            .join("pi-env-darwin-arm64")
            .join("pi-env");
        touch_file(&legacy);

        let target = remote(RemotePlatformKind::Darwin, RemoteArch::Arm64);
        assert_eq!(
            packaged_daemon_in(
                Some(&exe_path),
                None,
                Some(RemotePlatformKind::Linux),
                Some(RemoteArch::X64),
                &target,
            ),
            legacy
        );

        fs::remove_file(&legacy).unwrap();
        assert_eq!(
            packaged_daemon_in(
                Some(&exe_path),
                None,
                Some(RemotePlatformKind::Linux),
                Some(RemoteArch::X64),
                &target,
            ),
            exe_dir.join("pi-env-darwin-arm64").join("pi-env")
        );

        let override_dir = dir.path().join("custom");
        assert_eq!(
            packaged_daemon_in(
                Some(&exe_path),
                Some(&override_dir),
                Some(RemotePlatformKind::Linux),
                Some(RemoteArch::X64),
                &target,
            ),
            override_dir.join("pi-env-darwin-arm64").join("pi-env")
        );
    }
}

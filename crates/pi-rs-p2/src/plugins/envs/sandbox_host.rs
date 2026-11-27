//! Gondolin sandbox host launcher (`apps/p2/plugins/envs/sandbox-host.ts`):
//! embeds the upstream sandbox launcher bundle, deploys it over SSH to `~/.pi/p2/sandbox/<id>/`,
//! and runs one-shot lifecycle commands (`check`, `status`, `start`, `stop`, `reset`, `forget`)
//! plus `daemon` for `pi-env` over SSH.

use std::{
    collections::BTreeMap,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::LazyLock,
    time::Duration,
};

use base64::Engine;
use pi_rs_chord::{Error, Result};
use pi_rs_env::ssh::{RemotePlatform, RemotePlatformKind, SshTarget, ssh_arguments};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    config::{DEFAULT_DISK_GB, GondolinEnvConfig},
    secrets::SecretMeta,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const START_TIMEOUT: Duration = Duration::from_secs(660);

const FILES: &[(&str, &[u8])] = &[
    (
        "p2-sandbox.sh",
        include_bytes!("../../../assets/envs/sandbox/p2-sandbox.sh"),
    ),
    (
        "p2-sandbox.mjs",
        include_bytes!("../../../assets/envs/sandbox/p2-sandbox.mjs"),
    ),
    (
        "package.json",
        include_bytes!("../../../assets/envs/sandbox/host-package.json"),
    ),
    (
        "package-lock.json",
        include_bytes!("../../../assets/envs/sandbox/host-package-lock.json"),
    ),
];

static LAUNCHER_ID: LazyLock<String> = LazyLock::new(|| {
    let mut hash = Sha256::new();
    for (name, bytes) in FILES {
        hash.update(format!("{name}\0{}\0", bytes.len()).as_bytes());
        hash.update(bytes);
    }
    let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    hex.chars().take(16).collect()
});

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxStatusReport {
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSupport {
    pub runtime: String,
    pub detail: String,
}

pub fn launcher_id() -> &'static str {
    &LAUNCHER_ID
}

pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub fn run_launcher_command(directory: &str, args: &[&str]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 2);
    parts.push("sh".to_owned());
    parts.push(shell_quote(&format!("{directory}/p2-sandbox.sh")));
    for arg in args {
        parts.push(shell_quote(arg));
    }
    parts.join(" ")
}

/// Base64 JSON configuration passed to the sandbox launcher (`start` / `daemon`).
pub fn sandbox_argument(config: &GondolinEnvConfig, secrets: &[SecretMeta]) -> String {
    let mut obj = serde_json::Map::new();
    if let Some(ref image) = config.image {
        obj.insert("image".into(), json!(image));
    }
    obj.insert(
        "mounts".into(),
        json!(config.mounts.as_deref().unwrap_or(&[])),
    );
    obj.insert(
        "allowHosts".into(),
        json!(config.allow_hosts.as_deref().unwrap_or(&[])),
    );
    if let Some(cpus) = config.cpus {
        obj.insert("cpus".into(), json!(cpus));
    }
    if let Some(memory_mb) = config.memory_mb {
        obj.insert("memoryMb".into(), json!(memory_mb));
    }
    obj.insert(
        "diskGb".into(),
        json!(config.disk_gb.unwrap_or(DEFAULT_DISK_GB)),
    );
    if !secrets.is_empty() {
        obj.insert("secrets".into(), json!(secrets));
    }
    let json_bytes = serde_json::to_vec(&Value::Object(obj)).unwrap_or_else(|_| b"{}".to_vec());
    base64::engine::general_purpose::STANDARD.encode(json_bytes)
}

/// Configuration revision hash matching `apps/p2/plugins/envs/sandbox-host.ts` (`sandboxRevision`).
pub fn sandbox_revision(config: &GondolinEnvConfig, secrets: &[SecretMeta]) -> String {
    let arg = sandbox_argument(config, secrets);
    let hex: String = Sha256::digest(arg.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    hex.chars().take(16).collect()
}

/// Unique VM name on the host machine so separate p2 installations never collide.
pub fn sandbox_key(directory: &Path, name: &str) -> String {
    let hostname = nix_or_env_hostname();
    let mut hasher = Sha256::new();
    hasher.update(hostname.as_bytes());
    hasher.update(b"\0");
    hasher.update(directory.to_string_lossy().as_bytes());
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{}.{}", &hex[..8], name)
}

fn nix_or_env_hostname() -> String {
    #[cfg(unix)]
    {
        if let Ok(name) = nix::unistd::gethostname()
            && let Some(s) = name.to_str()
        {
            return s.to_owned();
        }
    }
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "localhost".into())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn run_on_host(
    target: &SshTarget,
    command: &str,
    input: Option<&str>,
    timeout: Duration,
) -> Result<String> {
    let ssh_bin = target.ssh.as_deref().unwrap_or("ssh");
    let args = ssh_arguments(target, true, Some(&target.known_hosts_file))
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let mut cmd = Command::new(ssh_bin);
    cmd.args(args)
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Invalid(format!("ssh spawn failed: {e}")))?;
    if let Some(payload) = input
        && let Some(mut stdin) = child.stdin.take()
    {
        let _ = stdin.write_all(payload.as_bytes());
    } else {
        drop(child.stdin.take());
    }
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::Invalid("ssh sandbox launcher timed out".into()));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(err) => return Err(Error::Invalid(format!("ssh wait failed: {err}"))),
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|e| Error::Invalid(format!("ssh output failed: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let code = output.status.code().unwrap_or(1);
        return Err(Error::Invalid(if stderr.is_empty() {
            format!("ssh exited with code {code}")
        } else {
            stderr
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Deploys the embedded sandbox launcher bundle to `<remote.home>/.pi/p2/sandbox/<id>/`.
pub fn deploy_launcher(target: &SshTarget, remote: &RemotePlatform) -> Result<String> {
    let root = format!("{}/.pi/p2/sandbox", remote.home);
    let directory = format!("{root}/{}", launcher_id());
    let mut script = String::new();
    script.push_str("set -e\n");
    script.push_str(&format!("d={}\n", shell_quote(&directory)));
    script.push_str(&format!("r={}\n", shell_quote(&root)));
    script.push_str("hash() { sha256sum \"$1\" 2>/dev/null | cut -d\" \" -f1; }\n");
    script.push_str("intact() {\n");
    script.push_str("\t[ -f \"$d/.complete\" ] || return 1\n");
    for (name, bytes) in FILES {
        script.push_str(&format!(
            "\t[ \"$(hash \"$d/{name}\")\" = {} ] || return 1\n",
            sha256_hex(bytes)
        ));
    }
    script.push_str("}\n");
    script.push_str("if ! intact; then\n");
    script.push_str("\tmkdir -p \"$r\"\n");
    script.push_str("\tt=$(mktemp -d \"$d.XXXXXX\")\n");
    for (name, bytes) in FILES {
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        script.push_str(&format!(
            "\tprintf %s {} | base64 -d > \"$t/{name}\"\n",
            shell_quote(&b64)
        ));
    }
    script.push_str("\tchmod 755 \"$t/p2-sandbox.sh\"\n");
    script.push_str("\ttouch \"$t/.complete\"\n");
    script.push_str("\trm -rf \"$d\"\n");
    script.push_str("\tmv \"$t\" \"$d\"\n");
    script.push_str("fi\n");
    script.push_str("touch \"$d\"\n");
    script.push_str("find \"$r\" -mindepth 1 -maxdepth 1 -type d -name \"????????????????\" -mtime +30 -exec rm -rf {} +\n");
    run_on_host(target, "sh -s", Some(&script), COMMAND_TIMEOUT)?;
    Ok(directory)
}

/// Executes a one-shot JSON command on a deployed launcher and parses the last line as JSON.
pub fn deployed_command(
    target: &SshTarget,
    deployed_dir: &str,
    args: &[&str],
    input: Option<&str>,
) -> Result<Value> {
    let timeout = if args.first().copied() == Some("start") {
        START_TIMEOUT
    } else {
        COMMAND_TIMEOUT
    };
    let output = run_on_host(
        target,
        &run_launcher_command(deployed_dir, args),
        input,
        timeout,
    )?;
    let line = output.trim().lines().last().unwrap_or("");
    serde_json::from_str::<Value>(line).map_err(|_| {
        let preview: String = line.chars().take(200).collect();
        Error::Invalid(format!(
            "Unexpected answer from the sandbox launcher: {preview}"
        ))
    })
}

pub fn check_sandbox_support(target: &SshTarget, remote: &RemotePlatform) -> SandboxSupport {
    if remote.platform != RemotePlatformKind::Linux {
        return SandboxSupport {
            runtime: "none".into(),
            detail: "managed via host".into(),
        };
    }
    let Ok(deployed) = deploy_launcher(target, remote) else {
        return SandboxSupport {
            runtime: "none".into(),
            detail: "could not check sandbox runtime".into(),
        };
    };
    match deployed_command(target, &deployed, &["check"], None) {
        Ok(report) => SandboxSupport {
            runtime: report["runtime"].as_str().unwrap_or("none").to_owned(),
            detail: report["detail"]
                .as_str()
                .unwrap_or("unavailable")
                .to_owned(),
        },
        Err(_) => SandboxSupport {
            runtime: "none".into(),
            detail: "could not check sandbox runtime".into(),
        },
    }
}

pub fn parse_status_report(
    report: &Value,
    config: &GondolinEnvConfig,
    secrets: &[SecretMeta],
) -> Result<SandboxStatusReport> {
    match report.get("state").and_then(Value::as_str) {
        Some("stopped") => Ok(SandboxStatusReport {
            state: "stopped".into(),
            started_at: None,
            cpus: None,
            memory_mb: None,
            revision: None,
            current: None,
        }),
        Some("running") => {
            let revision = report
                .get("revision")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let expected = sandbox_revision(config, secrets);
            let current = revision.as_deref() == Some(expected.as_str());
            Ok(SandboxStatusReport {
                state: "running".into(),
                started_at: report.get("startedAt").and_then(Value::as_u64),
                cpus: report
                    .get("cpus")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok()),
                memory_mb: report
                    .get("memoryMb")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok()),
                revision,
                current: Some(current),
            })
        }
        _ => Err(Error::Invalid(
            "Unexpected answer from the sandbox launcher".into(),
        )),
    }
}

pub async fn run_sandbox_command(
    directory: &Path,
    name: &str,
    config: &GondolinEnvConfig,
    target: SshTarget,
    command: &str,
    meta: Vec<SecretMeta>,
    values: BTreeMap<String, String>,
) -> Result<SandboxStatusReport> {
    let key = sandbox_key(directory, name);
    let config_clone = config.clone();
    let command = command.to_owned();
    tokio::task::spawn_blocking(move || -> Result<SandboxStatusReport> {
        let remote =
            pi_rs_env::ssh::detect_platform(&target).map_err(|e| Error::Invalid(e.to_string()))?;
        if remote.platform != RemotePlatformKind::Linux {
            return Err(Error::Invalid(format!(
                "Sandboxes need a Linux host; \"{}\" runs {:?}",
                config_clone.host, remote.platform
            )));
        }
        let deployed = deploy_launcher(&target, &remote)?;
        let argument = sandbox_argument(&config_clone, &meta);
        let input = if command == "start" {
            Some(serde_json::to_string(&values)?)
        } else {
            None
        };
        let raw = if command == "start" {
            deployed_command(
                &target,
                &deployed,
                &["start", &key, &argument],
                input.as_deref(),
            )?
        } else {
            deployed_command(&target, &deployed, &[&command, &key], None)?
        };
        parse_status_report(&raw, &config_clone, &meta)
    })
    .await
    .map_err(|e| Error::Invalid(e.to_string()))?
}

/// Builds the remote command vector that starts/attaches to `pi-env` inside a Gondolin sandbox VM.
pub fn sandbox_daemon_argv(
    directory: &Path,
    name: &str,
    config: &GondolinEnvConfig,
    target: &SshTarget,
    meta: &[SecretMeta],
    values: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let remote =
        pi_rs_env::ssh::detect_platform(target).map_err(|e| Error::Invalid(e.to_string()))?;
    if remote.platform != RemotePlatformKind::Linux {
        return Err(Error::Invalid(format!(
            "Sandboxes need a Linux host; \"{}\" runs {:?}",
            config.host, remote.platform
        )));
    }
    let daemon = pi_rs_env::ssh::deploy_daemon(target, &remote, None)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let deployed = deploy_launcher(target, &remote)?;
    let key = sandbox_key(directory, name);
    let argument = sandbox_argument(config, meta);
    let values_json = serde_json::to_string(values)?;
    let _ = deployed_command(
        target,
        &deployed,
        &["start", &key, &argument],
        Some(&values_json),
    )?;
    let mut cmd = Vec::new();
    cmd.push(target.ssh.clone().unwrap_or_else(|| "ssh".into()));
    cmd.extend(
        ssh_arguments(target, true, Some(&target.known_hosts_file))
            .map_err(|e| Error::Invalid(e.to_string()))?,
    );
    cmd.push(run_launcher_command(
        &deployed,
        &["daemon", &key, &argument, &daemon, "--"],
    ));
    Ok(cmd)
}

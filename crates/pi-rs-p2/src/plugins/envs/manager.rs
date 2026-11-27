//! Owner management of `envs.json`, SSH keys, host keys, connection tests, and directory browsing.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use pi_rs_chord::{Error, Result};
use pi_rs_durable::env::{FileKind, FileSystem};
use pi_rs_env::{
    Connection, RemoteExecutionEnv, RemoteExecutionEnvOptions,
    ssh::{
        SshConnectOptions, SshTarget, accept_host_key as ssh_accept_host_key, connect_ssh,
        forget_host_key as ssh_forget_host_key, local_daemon, scan_host_key, ssh_connection,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::plugins::{
    envs::{
        config::{
            EnvConfig, GondolinEnvConfig, SshEnvConfig, delete_env, env_revision, load_envs,
            save_env,
        },
        sandbox_host::{SandboxStatusReport, check_sandbox_support, run_sandbox_command},
        secrets::{SecretInfo, delete_secret, list_secrets, sandbox_secrets, save_secret},
    },
    locations::{BoxedFuture, DirectoryListing, EnvDirectoryManager},
};

const MAX_KEY_BYTES: usize = 16 * 1024;
const LISTING_SCAN: u64 = 10_000;
const LISTING_LIMIT: usize = 1_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvSummary {
    pub name: String,
    pub config: EnvConfig,
    pub sessions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test: Option<Value>,
}

pub type UsesFn = Arc<dyn Fn(&str) -> usize + Send + Sync>;

pub struct EnvManager {
    pub directory: PathBuf,
    uses: UsesFn,
    tests: Mutex<HashMap<String, Value>>,
}

impl EnvManager {
    pub fn new(directory: PathBuf, uses: UsesFn) -> Self {
        Self {
            directory,
            uses,
            tests: Mutex::new(HashMap::new()),
        }
    }

    pub async fn config(&self, name: &str) -> Result<EnvConfig> {
        let configs = load_envs(&self.directory).await?;
        configs
            .get(name)
            .cloned()
            .ok_or_else(|| Error::Invalid(format!("Environment \"{name}\" is not configured")))
    }

    pub async fn ssh_config(&self, name: &str) -> Result<SshEnvConfig> {
        match self.config(name).await? {
            EnvConfig::Ssh(cfg) => Ok(cfg),
            EnvConfig::Gondolin(_) => Err(Error::Invalid(format!(
                "\"{name}\" is a sandbox; manage its host's SSH settings instead"
            ))),
        }
    }

    pub fn key_path(&self, name: &str) -> PathBuf {
        self.directory.join("keys").join(name)
    }

    fn known_hosts_path(&self) -> PathBuf {
        self.directory.join("known_hosts")
    }

    fn ssh_target(&self, name: &str, cfg: &SshEnvConfig) -> SshTarget {
        let identity_file = cfg.identity_file.clone().or_else(|| {
            let default = self.key_path(name);
            default
                .exists()
                .then(|| default.to_string_lossy().into_owned())
        });
        SshTarget {
            host: cfg.host.clone(),
            user: cfg.user.clone(),
            port: cfg.port,
            identity_file,
            known_hosts_file: self.known_hosts_path().to_string_lossy().into_owned(),
            host_key_alias: format!("p2-{name}"),
            ssh: None,
            config_file: None,
        }
    }

    fn ssh_connect_options(
        &self,
        name: &str,
        cfg: &SshEnvConfig,
        binary: Option<PathBuf>,
    ) -> SshConnectOptions {
        let target = self.ssh_target(name, cfg);
        SshConnectOptions {
            host: target.host,
            user: target.user,
            port: target.port,
            identity_file: target.identity_file,
            known_hosts_file: target.known_hosts_file,
            host_key_alias: target.host_key_alias,
            ssh: None,
            config_file: None,
            binary,
            login_shell: cfg.login_shell.unwrap_or(false),
            on_log: None,
        }
    }

    pub async fn list(&self) -> Result<Vec<EnvSummary>> {
        let configs = load_envs(&self.directory).await?;
        let tests = self.tests.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Ok(configs
            .into_iter()
            .map(|(name, config)| {
                let key = format!("{name}@{}", env_revision(&config));
                let test = tests.get(&key).cloned();
                let sessions = (self.uses)(&name);
                EnvSummary {
                    name,
                    config,
                    sessions,
                    test,
                }
            })
            .collect())
    }

    pub async fn save(&self, name: &str, config: EnvConfig) -> Result<()> {
        save_env(&self.directory, name, config).await?;
        Ok(())
    }

    pub async fn delete(&self, name: &str, force: bool) -> Result<()> {
        let sessions = (self.uses)(name);
        if sessions > 0 && !force {
            return Err(Error::Invalid(format!(
                "Environment \"{name}\" is used by {sessions} session(s); pass force to delete"
            )));
        }
        let existing = self.config(name).await.ok();
        delete_env(&self.directory, name).await?;
        if let Some(EnvConfig::Gondolin(cfg)) = existing {
            if let Ok(target) = self.sandbox_host_target(&cfg.host).await
                && let Ok((meta, values)) = sandbox_secrets(&self.directory, name).await
            {
                let _ = run_sandbox_command(
                    &self.directory,
                    name,
                    &cfg,
                    target,
                    "forget",
                    meta,
                    values,
                )
                .await;
            }
            let _ = delete_secret(&self.directory, name, None).await;
        }
        Ok(())
    }

    async fn sandbox_host_target(&self, host_name: &str) -> Result<SshTarget> {
        let host_cfg = self.ssh_config(host_name).await?;
        Ok(self.ssh_target(host_name, &host_cfg))
    }

    async fn sandbox_command(&self, name: &str, command: &str) -> Result<SandboxStatusReport> {
        let EnvConfig::Gondolin(cfg) = self.config(name).await? else {
            return Err(Error::Invalid(format!("\"{name}\" is not a sandbox")));
        };
        let target = self.sandbox_host_target(&cfg.host).await?;
        let (meta, values) = sandbox_secrets(&self.directory, name).await?;
        run_sandbox_command(&self.directory, name, &cfg, target, command, meta, values).await
    }

    pub async fn sandbox_status(&self, name: &str) -> Result<SandboxStatusReport> {
        self.sandbox_command(name, "status").await
    }

    pub async fn sandbox_start(&self, name: &str) -> Result<SandboxStatusReport> {
        self.sandbox_command(name, "start").await
    }

    pub async fn sandbox_stop(&self, name: &str) -> Result<SandboxStatusReport> {
        self.sandbox_command(name, "stop").await
    }

    pub async fn sandbox_reset_disk(&self, name: &str) -> Result<SandboxStatusReport> {
        self.sandbox_command(name, "reset").await
    }

    pub async fn accept_host_key(&self, name: &str, lines: &[String]) -> Result<()> {
        let cfg = self.ssh_config(name).await?;
        let target = self.ssh_target(name, &cfg);
        let lines = lines.to_vec();
        tokio::task::spawn_blocking(move || ssh_accept_host_key(&target, &lines))
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?
            .map_err(|e| Error::Invalid(e.to_string()))
    }

    pub async fn forget_host_key(&self, name: &str) -> Result<()> {
        let cfg = self.ssh_config(name).await?;
        let target = self.ssh_target(name, &cfg);
        tokio::task::spawn_blocking(move || ssh_forget_host_key(&target))
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?
            .map_err(|e| Error::Invalid(e.to_string()))
    }

    pub async fn generate_key(&self, name: &str) -> Result<String> {
        let _ = self.ssh_config(name).await?;
        let key_path = self.key_path(name);
        let pub_path = PathBuf::from(format!("{}.pub", key_path.display()));
        if let Some(parent) = key_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let kp = key_path.clone();
        let pp = pub_path.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _ = std::fs::remove_file(&kp);
            let _ = std::fs::remove_file(&pp);
            let status = std::process::Command::new("ssh-keygen")
                .args(["-t", "ed25519", "-N", "", "-f"])
                .arg(&kp)
                .output()
                .map_err(|e| Error::Invalid(format!("ssh-keygen failed: {e}")))?;
            if !status.status.success() {
                return Err(Error::Invalid(
                    String::from_utf8_lossy(&status.stderr).trim().to_owned(),
                ));
            }
            Ok(())
        })
        .await
        .map_err(|e| Error::Invalid(e.to_string()))??;
        let pub_key = tokio::fs::read_to_string(&pub_path).await?;
        Ok(pub_key.trim().to_owned())
    }

    pub async fn upload_key(&self, name: &str, key: &str) -> Result<String> {
        let _ = self.ssh_config(name).await?;
        if key.len() > MAX_KEY_BYTES {
            return Err(Error::Invalid("Private key exceeds 16 KiB limit".into()));
        }
        let key_path = self.key_path(name);
        if let Some(parent) = key_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let kp = key_path.clone();
        let content = format!("{}\n", key.trim_end());
        let pub_key = tokio::task::spawn_blocking(move || -> Result<String> {
            std::fs::write(&kp, content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&kp, std::fs::Permissions::from_mode(0o600))?;
            }
            let out = std::process::Command::new("ssh-keygen")
                .args(["-y", "-f"])
                .arg(&kp)
                .output()
                .map_err(|e| Error::Invalid(format!("ssh-keygen failed: {e}")))?;
            if !out.status.success() {
                let _ = std::fs::remove_file(&kp);
                return Err(Error::Invalid(
                    String::from_utf8_lossy(&out.stderr).trim().to_owned(),
                ));
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        })
        .await
        .map_err(|e| Error::Invalid(e.to_string()))??;
        Ok(pub_key)
    }

    pub async fn public_key(&self, name: &str) -> Result<Option<String>> {
        let cfg = self.ssh_config(name).await?;
        let key_path = cfg
            .identity_file
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.key_path(name));
        let pub_path = PathBuf::from(format!("{}.pub", key_path.display()));
        match tokio::fs::read_to_string(&pub_path).await {
            Ok(text) => Ok(Some(text.trim().to_owned())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub async fn test(&self, name: &str) -> Result<Value> {
        let config = self.config(name).await?;
        let rev = env_revision(&config);
        let tested_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let started = Instant::now();

        let result = match &config {
            EnvConfig::Ssh(cfg) => {
                let target = self.ssh_target(name, cfg);
                let daemon = local_daemon().ok();
                let opts = self.ssh_connect_options(name, cfg, daemon);
                let conn_res = tokio::task::spawn_blocking(move || connect_ssh(opts))
                    .await
                    .map_err(|e| Error::Invalid(e.to_string()))?;
                match conn_res {
                    Ok((conn, remote)) => {
                        let info = conn.info();
                        let platform = format!("{:?}", remote.platform).to_lowercase();
                        let arch = format!("{:?}", remote.arch).to_lowercase();
                        let home = remote.home.clone();
                        let version = info
                            .as_ref()
                            .map(|i| i.version.clone())
                            .unwrap_or_else(|_| "1".into());
                        conn.close();
                        let target_for_sb = target.clone();
                        let remote_for_sb = remote.clone();
                        let sandboxes = tokio::task::spawn_blocking(move || {
                            check_sandbox_support(&target_for_sb, &remote_for_sb)
                        })
                        .await
                        .unwrap_or(
                            crate::plugins::envs::sandbox_host::SandboxSupport {
                                runtime: "none".into(),
                                detail: "could not check sandbox runtime".into(),
                            },
                        );
                        json!({
                            "ok": true,
                            "testedAt": tested_at,
                            "platform": platform,
                            "arch": arch,
                            "home": home,
                            "version": version,
                            "latencyMs": started.elapsed().as_millis() as u64,
                            "watchMode": "native",
                            "warnings": remote.warnings,
                            "sandboxes": sandboxes,
                        })
                    }
                    Err(err) => {
                        let host_key = scan_host_key(&target).ok().map(|scanned| {
                            json!({
                                "state": "unknown",
                                "fingerprints": scanned.fingerprints,
                                "lines": scanned.lines,
                            })
                        });
                        let mut out = json!({
                            "ok": false,
                            "testedAt": tested_at,
                            "error": err.to_string(),
                        });
                        if let Some(hk) = host_key {
                            out["hostKey"] = hk;
                        }
                        out
                    }
                }
            }
            EnvConfig::Gondolin(cfg) => {
                let conn_res = self.open_sandbox_connection(name, cfg).await;
                match conn_res {
                    Ok(conn) => {
                        let info = conn.info();
                        let version = info
                            .as_ref()
                            .map(|i| i.version.clone())
                            .unwrap_or_else(|_| "1".into());
                        let home = info
                            .as_ref()
                            .map(|i| i.home.clone())
                            .unwrap_or_else(|_| "/root".into());
                        conn.close();
                        json!({
                            "ok": true,
                            "testedAt": tested_at,
                            "platform": "linux",
                            "arch": "x86_64",
                            "home": home,
                            "version": version,
                            "latencyMs": started.elapsed().as_millis() as u64,
                            "watchMode": "native",
                            "warnings": [],
                        })
                    }
                    Err(err) => json!({
                        "ok": false,
                        "testedAt": tested_at,
                        "error": err.to_string(),
                    }),
                }
            }
        };

        self.tests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(format!("{name}@{rev}"), result.clone());
        Ok(result)
    }

    async fn open_sandbox_connection(
        &self,
        name: &str,
        cfg: &GondolinEnvConfig,
    ) -> Result<Arc<Connection>> {
        let target = self.sandbox_host_target(&cfg.host).await?;
        let (meta, values) = sandbox_secrets(&self.directory, name).await?;
        let directory = self.directory.clone();
        let name_owned = name.to_owned();
        let cfg_owned = cfg.clone();
        let conn = Arc::new(Connection::new(pi_rs_env::ConnectionOptions {
            command: pi_rs_env::CommandSource::Dynamic(Arc::new(move || {
                crate::plugins::envs::sandbox_host::sandbox_daemon_argv(
                    &directory,
                    &name_owned,
                    &cfg_owned,
                    &target,
                    &meta,
                    &values,
                )
                .map_err(|e| {
                    pi_rs_env::RemoteError::new(&json!({
                        "code": "spawn_error",
                        "message": e.to_string(),
                        "lost": true,
                    }))
                })
            })),
            on_log: None,
        }));
        let conn_check = conn.clone();
        tokio::task::spawn_blocking(move || conn_check.info())
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?
            .map_err(|e| Error::Invalid(e.to_string()))?;
        Ok(conn)
    }

    pub async fn list_remote_directory(&self, name: &str, path: &str) -> Result<DirectoryListing> {
        let cfg = self.ssh_config(name).await?;
        let daemon = local_daemon().ok();
        let opts = self.ssh_connect_options(name, &cfg, daemon);
        let conn: Arc<Connection> = Arc::new(ssh_connection(opts).connection);
        let env = RemoteExecutionEnv::new(RemoteExecutionEnvOptions {
            id: name.to_owned(),
            cwd: "/".into(),
            connection: conn.clone(),
            shell_path: cfg.shell_path,
            shell_env: cfg.shell_env.unwrap_or_default().into_iter().collect(),
            watch: Default::default(),
        });
        let ctx = pi_rs_chord::context::Context::background();
        let abs = env
            .absolute_path(path, &ctx)
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let entries = env
            .list_dir(&abs, &ctx)
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let scan_truncated = entries.len() > LISTING_SCAN as usize;
        let mut dirs: Vec<String> = entries
            .into_iter()
            .take(LISTING_SCAN as usize)
            .filter(|e| e.kind == FileKind::Directory)
            .map(|e| format!("{}/{}", abs.trim_end_matches('/'), e.name))
            .collect();
        dirs.sort();
        let truncated = scan_truncated || dirs.len() > LISTING_LIMIT;
        dirs.truncate(LISTING_LIMIT);
        let parent = Path::new(&abs)
            .parent()
            .filter(|p| *p != Path::new(&abs))
            .map(|p| p.to_string_lossy().into_owned());
        conn.close();
        Ok(DirectoryListing {
            path: abs,
            parent,
            directories: dirs,
            truncated,
        })
    }

    pub async fn secrets(&self, sandbox: &str) -> Result<Vec<SecretInfo>> {
        let _ = self.config(sandbox).await?;
        list_secrets(&self.directory, sandbox).await
    }

    pub async fn set_secret(
        &self,
        sandbox: &str,
        name: &str,
        hosts: Vec<String>,
        preset: Option<String>,
        value: Option<String>,
        add_hosts: bool,
    ) -> Result<()> {
        let mut cfg = match self.config(sandbox).await? {
            EnvConfig::Gondolin(g) => g,
            EnvConfig::Ssh(_) => {
                return Err(Error::Invalid(format!("\"{sandbox}\" is not a sandbox")));
            }
        };
        if add_hosts && let Some(ref mut allowed) = cfg.allow_hosts {
            let missing: Vec<String> = hosts
                .iter()
                .filter(|h| !allowed.contains(h))
                .cloned()
                .collect();
            if !missing.is_empty() {
                allowed.extend(missing);
                let updated = cfg.clone();
                self.save(sandbox, EnvConfig::Gondolin(updated)).await?;
            }
        }
        save_secret(
            &self.directory,
            sandbox,
            name,
            hosts,
            preset,
            value,
            cfg.allow_hosts.as_deref(),
        )
        .await
    }

    pub async fn delete_secret(&self, sandbox: &str, name: &str) -> Result<()> {
        let _ = self.config(sandbox).await?;
        delete_secret(&self.directory, sandbox, Some(name)).await
    }
}

impl EnvDirectoryManager for EnvManager {
    fn list_directory<'a>(
        &'a self,
        env: &'a str,
        path: &'a str,
    ) -> BoxedFuture<'a, DirectoryListing> {
        let env = env.to_owned();
        let path = path.to_owned();
        Box::pin(async move { self.list_remote_directory(&env, &path).await })
    }
}

//! Worker facet for `envs`: installs `EnvironmentProvider` on `worker.registries.environments`.

use std::{path::PathBuf, sync::Arc};

use pi_rs_chord::{
    Result,
    host::{Facet, define_facet},
};
use pi_rs_env::{
    CommandSource, Connection, ConnectionOptions, RemoteError,
    ssh::{SshConnectOptions, SshTarget, local_daemon, ssh_connection},
};
use serde_json::json;

use crate::{
    api::v1::worker::worker_of,
    plugins::envs::{
        config::{EnvConfig, is_sandbox, parse_envs},
        sandbox_host::sandbox_daemon_argv,
        secrets::sandbox_secrets,
    },
    runtime::worker_environment::{EnvironmentProvider, Machine, WorkerEnvironmentError},
};

struct WorkerEnvProvider {
    data_dir: PathBuf,
}

pub(crate) fn env_provider(data_dir: PathBuf) -> Arc<dyn EnvironmentProvider> {
    Arc::new(WorkerEnvProvider { data_dir })
}

impl WorkerEnvProvider {
    fn load_sync(
        &self,
    ) -> std::result::Result<crate::plugins::envs::config::EnvsConfig, WorkerEnvironmentError> {
        let path = self.data_dir.join("envs.json");
        match std::fs::read_to_string(path) {
            Ok(content) => parse_envs(&content)
                .map_err(|e| WorkerEnvironmentError::Provider(e.to_string().into())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
            Err(err) => Err(WorkerEnvironmentError::Provider(Box::new(err))),
        }
    }
}

impl EnvironmentProvider for WorkerEnvProvider {
    fn open(&self, name: &str) -> std::result::Result<Machine, WorkerEnvironmentError> {
        let configs = self.load_sync()?;
        let config = configs.get(name).ok_or_else(|| {
            WorkerEnvironmentError::Provider(
                format!("Environment \"{name}\" is not configured; refusing local execution")
                    .into(),
            )
        })?;
        match config {
            EnvConfig::Ssh(cfg) => {
                let default_key = self.data_dir.join("keys").join(name);
                let identity_file = cfg.identity_file.clone().or_else(|| {
                    default_key
                        .exists()
                        .then(|| default_key.to_string_lossy().into_owned())
                });
                let binary = local_daemon().ok();
                let opts = SshConnectOptions {
                    host: cfg.host.clone(),
                    user: cfg.user.clone(),
                    port: cfg.port,
                    identity_file,
                    known_hosts_file: self
                        .data_dir
                        .join("known_hosts")
                        .to_string_lossy()
                        .into_owned(),
                    host_key_alias: format!("p2-{name}"),
                    ssh: None,
                    config_file: None,
                    binary,
                    login_shell: cfg.login_shell.unwrap_or(false),
                    on_log: None,
                };
                let connection = Arc::new(ssh_connection(opts).connection);
                Ok(Machine::Remote {
                    id: name.to_owned(),
                    connection,
                    shell_path: cfg.shell_path.clone(),
                })
            }
            EnvConfig::Gondolin(cfg) => {
                let EnvConfig::Ssh(host_cfg) = configs.get(&cfg.host).ok_or_else(|| {
                    WorkerEnvironmentError::Provider(
                        format!(
                            "Sandbox {name} runs on \"{}\", which is not an SSH environment",
                            cfg.host
                        )
                        .into(),
                    )
                })?
                else {
                    return Err(WorkerEnvironmentError::Provider(
                        format!(
                            "Sandbox {name} runs on \"{}\", which is not an SSH environment",
                            cfg.host
                        )
                        .into(),
                    ));
                };
                let default_key = self.data_dir.join("keys").join(&cfg.host);
                let identity_file = host_cfg.identity_file.clone().or_else(|| {
                    default_key
                        .exists()
                        .then(|| default_key.to_string_lossy().into_owned())
                });
                let target = SshTarget {
                    host: host_cfg.host.clone(),
                    user: host_cfg.user.clone(),
                    port: host_cfg.port,
                    identity_file,
                    known_hosts_file: self
                        .data_dir
                        .join("known_hosts")
                        .to_string_lossy()
                        .into_owned(),
                    host_key_alias: format!("p2-{}", cfg.host),
                    ssh: None,
                    config_file: None,
                };
                let directory = self.data_dir.clone();
                let sb_name = name.to_owned();
                let sb_cfg = cfg.clone();
                let conn = Arc::new(Connection::new(ConnectionOptions {
                    command: CommandSource::Dynamic(Arc::new(move || {
                        let (meta, values) = tokio::task::block_in_place(|| {
                            tokio::runtime::Handle::current()
                                .block_on(sandbox_secrets(&directory, &sb_name))
                        })
                        .map_err(|e| {
                            RemoteError::new(&json!({
                                "code": "spawn_error",
                                "message": e.to_string(),
                                "lost": true,
                            }))
                        })?;
                        sandbox_daemon_argv(&directory, &sb_name, &sb_cfg, &target, &meta, &values)
                            .map_err(|e| {
                                RemoteError::new(&json!({
                                    "code": "spawn_error",
                                    "message": e.to_string(),
                                    "lost": true,
                                }))
                            })
                    })),
                    on_log: None,
                }));
                Ok(Machine::Remote {
                    id: name.to_owned(),
                    connection: conn,
                    shell_path: None,
                })
            }
        }
    }

    fn sandbox(&self, name: &str) -> std::result::Result<bool, WorkerEnvironmentError> {
        let configs = self.load_sync()?;
        Ok(is_sandbox(&configs, name))
    }
}

pub fn worker_facet() -> Result<Facet> {
    Ok(define_facet(move |facet_host| {
        let host = facet_host.host();
        let worker = worker_of(&host)?;
        worker
            .registries
            .environments
            .set_remote(Some(Arc::new(WorkerEnvProvider {
                data_dir: worker.init.data_dir.clone(),
            })));
        Ok(())
    }))
}

//! Worker facet for `envs`: installs `EnvironmentProvider` on `worker.registries.environments`.

use std::{path::PathBuf, sync::Arc};

use pi_rs_chord::{
    Result,
    host::{Facet, define_facet},
};
use pi_rs_env::ssh::{SshConnectOptions, local_daemon, ssh_connection};

use crate::{
    api::v1::worker::worker_of,
    plugins::envs::config::{EnvConfig, is_sandbox, parse_envs},
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
            EnvConfig::Gondolin(_) => Err(WorkerEnvironmentError::Provider(
                format!("Sandbox \"{name}\" requires host launcher").into(),
            )),
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

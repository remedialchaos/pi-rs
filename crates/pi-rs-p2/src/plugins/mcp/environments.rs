//! Remote subprocess transports share their pi-env connection by environment and revision.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};

use pi_rs_chord::{Error, Result, data::ChordError, host::Host};
use pi_rs_env::{
    Connection, ProcessOptions, RemoteProcess, SshConnectOptions, node_platform, open_process,
    ssh_connection,
};
use pi_rs_mcp::{
    McpPipe, McpPipeOpener, PipeData, PipeExited, PipeTransport, PipeTransportOptions, Root,
};
use serde_json::{Value, json};
use tokio::sync::watch;
use url::Url;

use crate::api::v1::server::{server_of, service_of};

use super::{
    placement::Connect,
    transports::{ConnectRemote, Opened, expand},
    windows_command,
};

pub const SERVICE: &str = "p2.mcp-environments";
pub const CONNECTION_SERVICE: &str = "p2.environment-connector";

/// The environment plugin supplies non-SSH launchers without exposing their credentials.
pub type ConnectEnvironment = Arc<dyn Fn(&str, &Value) -> Result<Connection> + Send + Sync>;

type Connections = Arc<Mutex<HashMap<(String, String), Weak<Environment>>>>;

struct Environment {
    connection: Arc<Connection>,
}

impl Drop for Environment {
    fn drop(&mut self) {
        self.connection.close();
    }
}

pub(super) fn connector(host: &Host) -> ConnectRemote {
    let host = host.downgrade();
    let connections: Connections = Arc::default();
    Arc::new(move |connect| {
        let (host, connections) = (host.clone(), connections.clone());
        Box::pin(async move {
            let Connect::Remote {
                environment,
                config,
                revision,
                command,
                args,
                env,
                cwd,
                base,
                host_variables,
            } = connect
            else {
                return Err(Error::Invalid("Expected a remote MCP placement".into()));
            };
            let host = host.upgrade().ok_or(Error::Closed)?;
            let data = PathBuf::from(&server_of(&host)?.info.data_dir);
            let additional = service_of::<ConnectEnvironment>(&host, CONNECTION_SERVICE).ok();
            let (shared, info) = tokio::task::spawn_blocking(move || {
                let key = (environment.clone(), revision);
                let shared = {
                    let mut connections = connections
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    connections.retain(|_, entry| entry.strong_count() != 0);
                    match connections.get(&key).and_then(Weak::upgrade) {
                        Some(connection) => connection,
                        None => {
                            let connection = if let Some(connect) = additional {
                                connect(&environment, &config)?
                            } else {
                                ssh(&data, &environment, &config)?
                            };
                            let shared = Arc::new(Environment {
                                connection: Arc::new(connection),
                            });
                            connections.insert(key, Arc::downgrade(&shared));
                            shared
                        }
                    }
                };
                let info = shared.connection.info().map_err(invalid)?;
                if !info.has_feature("process") {
                    return Err(Error::from(ChordError::new(
                        "daemon_too_old",
                        format!("the pi-env daemon on {environment} is too old; update it"),
                    )));
                }
                Ok((shared, info))
            })
            .await
            .map_err(invalid)??;
            let windows = node_platform(&info).0 == "win32";
            let expand_home = |value: &str| {
                if value == "~" {
                    info.home.clone()
                } else if let Some(relative) = value.strip_prefix("~/") {
                    resolve(&info.home, relative, windows)
                } else {
                    value.to_owned()
                }
            };
            let cwd = resolve(
                base.as_deref().unwrap_or(&info.home),
                &expand_home(&cwd),
                windows,
            );
            let command = expand_home(&command);
            let args = args
                .iter()
                .map(|argument| expand_home(argument))
                .collect::<Vec<_>>();
            let (argv, launcher_env) = if windows {
                windows_command::launch(command, args)
            } else {
                (
                    std::iter::once(command).chain(args).collect(),
                    Default::default(),
                )
            };
            let mut env = if host_variables { expand(&env)? } else { env };
            env.extend(launcher_env);
            let roots = base
                .as_deref()
                .and_then(|base| root(base, windows))
                .into_iter()
                .collect();
            let open: McpPipeOpener = Arc::new(move |handlers| {
                let options = ProcessOptions {
                    cwd: cwd.clone(),
                    argv: argv.clone(),
                    env: Some(
                        env.iter()
                            .map(|(key, value)| (key.clone(), json!(value)))
                            .collect(),
                    ),
                    inherit_env: None,
                    on_stdout: handlers.on_stdout,
                    on_stderr: Some(handlers.on_stderr),
                };
                let environment = shared.clone();
                Box::pin(async move {
                    tokio::task::spawn_blocking(
                        move || -> std::result::Result<Arc<dyn McpPipe>, String> {
                            let process = open_process(environment.connection.clone(), options)
                                .map_err(|error| error.to_string())?;
                            Ok(Arc::new(Pipe::new(process, environment)))
                        },
                    )
                    .await
                    .map_err(|error| error.to_string())?
                })
            });
            let transport = PipeTransport::new(PipeTransportOptions {
                open,
                max_message_bytes: None,
                max_stderr_bytes: None,
                on_stderr: None,
                close_timeout_ms: None,
            });
            let opened: Opened = (transport, roots);
            Ok(opened)
        })
    })
}

fn ssh(data: &Path, name: &str, config: &Value) -> Result<Connection> {
    if config["type"] != "ssh" {
        return Err(Error::Invalid(format!(
            "No environment launcher is registered for {name}"
        )));
    }
    let field = |field| config[field].as_str().map(str::to_owned);
    let options = SshConnectOptions {
        host: field("host")
            .ok_or_else(|| Error::Invalid(format!("Missing SSH host for {name}")))?,
        user: field("user"),
        port: config["port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok()),
        identity_file: field("identityFile"),
        known_hosts_file: data
            .join("ssh")
            .join("known_hosts")
            .to_string_lossy()
            .into_owned(),
        host_key_alias: format!("pi-env-{name}"),
        ssh: None,
        config_file: None,
        binary: None,
        login_shell: config["loginShell"] == true,
        on_log: None,
    };
    Ok(ssh_connection(options).connection)
}

struct Pipe {
    process: Arc<RemoteProcess>,
    _environment: Arc<Environment>,
    exited: watch::Receiver<Option<std::result::Result<(), String>>>,
}

impl Pipe {
    fn new(process: RemoteProcess, environment: Arc<Environment>) -> Self {
        let process = Arc::new(process);
        let (publish, exited) = watch::channel(None);
        let wait = process.clone();
        tokio::task::spawn_blocking(move || {
            let result = wait.wait().map(|_| ()).map_err(|error| error.to_string());
            publish.send_replace(Some(result));
        });
        Self {
            process,
            _environment: environment,
            exited,
        }
    }
}

impl McpPipe for Pipe {
    fn write(&self, data: PipeData) {
        self.process.write(data.as_bytes());
    }
    fn close_stdin(&self) {
        self.process.close_stdin();
    }
    fn close(&self) {
        self.process.close();
    }
    fn exited(&self) -> PipeExited {
        let mut exited = self.exited.clone();
        Box::pin(async move {
            loop {
                if let Some(result) = exited.borrow_and_update().clone() {
                    return result;
                }
                exited.changed().await.map_err(|error| error.to_string())?;
            }
        })
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        self.process.close();
    }
}

fn resolve(base: &str, value: &str, windows: bool) -> String {
    let value = if windows {
        value.replace('\\', "/")
    } else {
        value.to_owned()
    };
    let base = if windows {
        base.replace('\\', "/")
    } else {
        base.to_owned()
    };
    let drive = |path: &str| path.as_bytes().get(1) == Some(&b':');
    let joined = if windows && value.starts_with('/') && !value.starts_with("//") && drive(&base) {
        format!("{}{value}", &base[..2])
    } else if value.starts_with('/') || (windows && drive(&value)) {
        value
    } else {
        format!("{base}/{value}")
    };
    let (prefix, path) = if windows && drive(&joined) {
        (&joined[..2], &joined[2..])
    } else if windows && joined.starts_with("//") {
        ("/", joined.as_str())
    } else {
        ("", joined.as_str())
    };
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if prefix != "/" || parts.len() > 2 => {
                parts.pop();
            }
            ".." => {}
            _ => parts.push(part),
        }
    }
    let full = format!("{prefix}/{}", parts.join("/"));
    if windows {
        full.replace('/', "\\")
    } else {
        full
    }
}

fn root(path: &str, windows: bool) -> Option<Root> {
    let uri = if windows {
        let path = path.replace('\\', "/");
        let mut url = Url::parse("file:///").ok()?;
        if let Some(parts) = path.strip_prefix("//") {
            let (server, tail) = parts.split_once('/')?;
            url.set_host(Some(server)).ok()?;
            url.set_path(&format!("/{tail}"));
        } else {
            url.set_path(&format!("/{path}"));
        }
        url
    } else {
        Url::from_file_path(path).ok()?
    };
    Some(Root {
        uri: uri.into(),
        name: None,
        extra: Default::default(),
    })
}

fn invalid(error: impl std::fmt::Display) -> Error {
    Error::Invalid(error.to_string())
}

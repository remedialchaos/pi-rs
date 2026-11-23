//! Select the MCP instances each authorized session location may use.

use std::{path::PathBuf, time::Duration};

use indexmap::{IndexMap, IndexSet};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::runtime::{
    location::{Location, location_text},
    session_lock::SessionLock,
};

use super::{
    config::{OwnerConfig, OwnerServer, RunOn},
    leases::{Lease, ProjectSource},
    project::{ProjectConfig, parse_project_config},
};

#[derive(Clone, Debug)]
pub struct InstanceSpec {
    pub id: String,
    pub server: String,
    pub source: &'static str,
    pub location: Option<Location>,
    pub runs_on: Value,
    pub connect: Connect,
    pub timeout: Option<Duration>,
    pub description: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Connect {
    Host {
        command: String,
        args: Vec<String>,
        env: IndexMap<String, String>,
        cwd: PathBuf,
        roots: Vec<String>,
        host_variables: bool,
    },
    Remote {
        environment: String,
        config: Value,
        revision: String,
        command: String,
        args: Vec<String>,
        env: IndexMap<String, String>,
        cwd: String,
        base: Option<String>,
        host_variables: bool,
    },
    Http {
        url: String,
        headers: IndexMap<String, String>,
        oauth: Option<OAuthServer>,
    },
    Unplaced {
        reason: String,
    },
}

#[derive(Clone, Debug)]
pub struct OAuthServer {
    pub name: String,
    pub url: String,
    pub config: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct PlacedLease {
    pub lease: Lease,
    pub servers: IndexMap<String, String>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Placement {
    pub instances: IndexMap<String, InstanceSpec>,
    pub leases: Vec<PlacedLease>,
    pub kept: IndexSet<String>,
}

pub fn place(
    config: &OwnerConfig,
    leases: &[Lease],
    envs: &IndexMap<String, Value>,
    work: &IndexMap<String, Vec<Option<String>>>,
    locks: &IndexMap<String, SessionLock>,
) -> Placement {
    let mut placement = Placement::default();
    for server in &config.servers {
        let (_, enabled, shared) = owner_traits(server);
        if enabled && shared {
            let spec = owner_spec(server, None, envs);
            placement.kept.insert(spec.id.clone());
            placement.instances.insert(spec.id.clone(), spec);
        }
    }
    for lease in leases {
        let location = &lease.location;
        let mut placed = PlacedLease {
            lease: lease.clone(),
            servers: IndexMap::new(),
            errors: Vec::new(),
        };
        if !work
            .get(&lease.session)
            .is_some_and(|envs| envs.contains(&location.env))
        {
            placed.errors.push(format!(
                "{}: not where this session works",
                location_text(location)
            ));
            placement.leases.push(placed);
            continue;
        }
        let project = match &lease.project {
            ProjectSource::File { text } => parse_project_config(text, config),
            ProjectSource::Unreadable { error } => ProjectConfig {
                errors: vec![format!(".pi/mcp.json: {error}")],
                ..Default::default()
            },
            ProjectSource::None => ProjectConfig::default(),
        };
        let lock = locks.get(&lease.session);
        let mut add = |spec: InstanceSpec| {
            placed.servers.insert(spec.server.clone(), spec.id.clone());
            placement.instances.insert(spec.id.clone(), spec);
        };
        for server in &config.servers {
            let (name, enabled, _) = owner_traits(server);
            if project.enabled.get(name).copied().unwrap_or(enabled)
                && lock.is_none_or(|lock| lock.mcp.iter().any(|allowed| allowed == name))
            {
                add(owner_spec(server, Some(lease), envs));
            }
        }
        if lock.is_none_or(|lock| lock.project_mcp) {
            for server in project.servers {
                add(stdio_spec(
                    &server.name,
                    "project",
                    Some(lease),
                    &RunOn::Location,
                    &server.config,
                    envs,
                ));
            }
        }
        placed.errors = project
            .errors
            .into_iter()
            .map(|error| format!("{}: {error}", location_text(location)))
            .collect();
        placement.leases.push(placed);
    }
    placement
}

fn owner_traits(server: &OwnerServer) -> (&str, bool, bool) {
    match server {
        OwnerServer::Stdio {
            name,
            enabled,
            shared,
            ..
        }
        | OwnerServer::Http {
            name,
            enabled,
            shared,
            ..
        } => (name, *enabled, *shared),
    }
}

fn owner_spec(
    server: &OwnerServer,
    lease: Option<&Lease>,
    envs: &IndexMap<String, Value>,
) -> InstanceSpec {
    let (name, _, shared) = owner_traits(server);
    let owned = if shared { None } else { lease };
    match server {
        OwnerServer::Stdio { config, run_on, .. } => {
            stdio_spec(name, "owner", owned, run_on, config, envs)
        }
        OwnerServer::Http { config, .. } => {
            let timeout = timeout(config);
            let url = config["url"].as_str().unwrap_or_default().to_owned();
            let headers = string_map(&config["headers"]);
            let mut picked = json!({"url":url,"headers":headers});
            if let Some(oauth) = config.get("oauth") {
                picked["oauth"] = oauth.clone();
            }
            picked["timeoutMs"] = timeout_ms(timeout);
            InstanceSpec {
                id: hash(&json!([
                    "http",
                    name,
                    owned.map(|lease| &lease.session),
                    hash(&picked)
                ])),
                server: name.into(),
                source: "owner",
                location: None,
                runs_on: json!("host"),
                connect: Connect::Http {
                    url,
                    headers,
                    oauth: oauth_server(server),
                },
                timeout,
                description: description(config),
            }
        }
    }
}

pub fn oauth_server(server: &OwnerServer) -> Option<OAuthServer> {
    let OwnerServer::Http { name, config, .. } = server else {
        return None;
    };
    if config["headers"].as_object().is_some_and(|headers| {
        headers
            .keys()
            .any(|key| key.eq_ignore_ascii_case("authorization"))
    }) {
        return None;
    }
    Some(OAuthServer {
        name: name.clone(),
        url: config["url"].as_str()?.to_owned(),
        config: config.get("oauth").cloned(),
    })
}

fn stdio_spec(
    server: &str,
    source: &'static str,
    lease: Option<&Lease>,
    run_on: &RunOn,
    config: &Value,
    envs: &IndexMap<String, Value>,
) -> InstanceSpec {
    let location = lease.map(|lease| &lease.location);
    let machine = match run_on {
        RunOn::Location => location.and_then(|location| location.env.as_deref()),
        RunOn::Host => None,
        RunOn::Env(env) => Some(env.as_str()),
    };
    let base = location
        .filter(|location| location.env.as_deref() == machine)
        .map(|location| location.cwd.as_str());
    let command = config["command"].as_str().unwrap_or_default();
    let args: Vec<String> = config["args"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|arg| arg.as_str().map(str::to_owned))
        .collect();
    let env = string_map(&config["env"]);
    let cwd = config["cwd"].as_str();
    let timeout = timeout(config);
    let configuration = hash(
        &json!({"command":command,"args":args,"env":env,"cwd":cwd,"timeoutMs":timeout_ms(timeout)}),
    );
    let owner = lease.map(|lease| json!([lease.session, lease.location]));
    let host_variables = source == "owner";
    let (id, runs_on, connect) = if let Some(machine) = machine {
        let runs_on = json!({"env":machine});
        let revision = envs.get(machine).map(|config| {
            let own = hash(config);
            if config["type"] == "ssh" {
                own
            } else {
                let host = config["host"]
                    .as_str()
                    .and_then(|host| envs.get(host))
                    .map(hash)
                    .unwrap_or_default();
                format!("{own}+{host}")
            }
        });
        let id = hash(&json!([
            "stdio",
            source,
            server,
            owner,
            runs_on,
            revision,
            configuration
        ]));
        let connect = match (envs.get(machine), revision) {
            (Some(config), Some(revision)) => Connect::Remote {
                environment: machine.into(),
                config: config.clone(),
                revision,
                command: command.into(),
                args,
                env,
                cwd: cwd.unwrap_or(".").into(),
                base: base.map(str::to_owned),
                host_variables,
            },
            _ => Connect::Unplaced {
                reason: format!("environment \"{machine}\" is not in envs.json"),
            },
        };
        (id, runs_on, connect)
    } else {
        let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let expand = |path: &str| {
            if path == "~" {
                home.clone()
            } else if let Some(tail) = path.strip_prefix("~/") {
                home.join(tail)
            } else {
                PathBuf::from(path)
            }
        };
        let cwd = base
            .map_or_else(|| home.clone(), PathBuf::from)
            .join(expand(cwd.unwrap_or(".")));
        let cwd = crate::runtime::location::join_path(&cwd.to_string_lossy(), "");
        let runs_on = json!("host");
        let id = hash(&json!([
            "stdio",
            source,
            server,
            owner,
            runs_on,
            configuration
        ]));
        let connect = Connect::Host {
            command: expand(command).to_string_lossy().into_owned(),
            args: args
                .iter()
                .map(|arg| expand(arg).to_string_lossy().into_owned())
                .collect(),
            env,
            cwd: cwd.into(),
            roots: base
                .and_then(|base| url::Url::from_file_path(base).ok())
                .into_iter()
                .map(|url| url.to_string())
                .collect(),
            host_variables,
        };
        (id, runs_on, connect)
    };
    InstanceSpec {
        id,
        server: server.into(),
        source,
        location: location.cloned(),
        runs_on,
        connect,
        timeout,
        description: description(config),
    }
}

fn string_map(value: &Value) -> IndexMap<String, String> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
        .collect()
}

fn description(config: &Value) -> Option<String> {
    config["description"]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn timeout(config: &Value) -> Option<Duration> {
    config["timeout"]
        .as_f64()
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
}

fn timeout_ms(timeout: Option<Duration>) -> Value {
    timeout
        .map_or(Value::Null, |timeout| json!(timeout.as_secs_f64() * 1000.0))
        .as_f64()
        .map_or(Value::Null, |ms| {
            if ms.fract() == 0.0 {
                json!(ms as u64)
            } else {
                json!(ms)
            }
        })
}

fn hash(value: &Value) -> String {
    let digest = Sha256::digest(value.to_string());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::mcp::config::parse_owner_config;

    #[test]
    fn placement_enforces_work_locations_and_session_lock() -> Result<(), String> {
        let owner = parse_owner_config(
            r#"{"mcpServers":{"docs":{"url":"https://example.test/mcp"},"extra":{"command":"extra"}}}"#,
        )?;
        let chicago = Lease {
            session: "sample".into(),
            generation: 1,
            incarnation: "one".into(),
            location: Location {
                cwd: "/workspace".into(),
                env: None,
            },
            project: ProjectSource::File {
                text: r#"{"mcpServers":{"project":{"command":"run"}}}"#.into(),
            },
        };
        let mut wrong = chicago.clone();
        wrong.location.env = Some("elsewhere".into());
        let work = IndexMap::from([("sample".into(), vec![None])]);
        let locks = IndexMap::from([(
            "sample".into(),
            SessionLock {
                mcp: vec!["docs".into()],
                project_mcp: false,
                instructions: false,
                capabilities: vec![],
            },
        )]);
        let list = place(&owner, &[chicago, wrong], &IndexMap::new(), &work, &locks);
        assert_eq!(
            list.leases[0]
                .servers
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["docs"]
        );
        assert!(list.leases[1].servers.is_empty());
        assert!(list.leases[1].errors[0].contains("not where this session works"));
        assert_eq!(list.kept.len(), 1);
        Ok(())
    }
}

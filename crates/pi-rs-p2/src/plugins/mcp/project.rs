//! Project configuration can add location-local stdio servers and toggle owner servers.

use indexmap::IndexMap;
use pi_rs_mcp::{
    McpServerConfigOptions, McpServerConfigResult, is_mcp_server_override, mcp_namespace,
    validate_mcp_server_config,
};
use serde_json::Value;

use super::config::{OwnerConfig, OwnerServer, server_entries};

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectServer {
    pub name: String,
    pub config: Value,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProjectConfig {
    pub servers: Vec<ProjectServer>,
    pub enabled: IndexMap<String, bool>,
    pub errors: Vec<String>,
}

pub fn parse_project_config(text: &str, owner: &OwnerConfig) -> ProjectConfig {
    let mut project = ProjectConfig::default();
    let entries = match server_entries(text, ".pi/mcp.json") {
        Ok(entries) => entries,
        Err(error) => {
            project.errors.push(error);
            return project;
        }
    };
    let mut names: IndexMap<String, String> = owner
        .servers
        .iter()
        .map(|server| {
            let name = match server {
                OwnerServer::Stdio { name, .. } | OwnerServer::Http { name, .. } => name,
            };
            (mcp_namespace(name), name.clone())
        })
        .collect();
    for (name, raw) in entries {
        let key = mcp_namespace(&name);
        let taken = names.get(&key);
        match project_entry(&name, &raw, taken == Some(&name)) {
            Ok(Entry::Override(Some(enabled))) => {
                project.enabled.insert(name, enabled);
            }
            Ok(Entry::Override(None) | Entry::Disabled) => {}
            Ok(Entry::Stdio(config)) => {
                if let Some(taken) = taken {
                    project.errors.push(format!(
                        ".pi/mcp.json: server \"{name}\": its tools would be named like those of \"{taken}\""
                    ));
                    continue;
                }
                names.insert(key, name.clone());
                project.servers.push(ProjectServer { name, config });
            }
            Err(error) => project.errors.push(format!(".pi/mcp.json: {error}")),
        }
    }
    project
}

enum Entry {
    Override(Option<bool>),
    Disabled,
    Stdio(Value),
}

fn project_entry(name: &str, raw: &Value, defined: bool) -> Result<Entry, String> {
    let refused = |reason: &str| format!("server \"{name}\": {reason}");
    if let Some(fields) = raw.as_object()
        && is_mcp_server_override(raw)
    {
        if !defined {
            return Err(refused(
                "needs \"command\", or a server of the owner's mcp.json to override",
            ));
        }
        if fields.keys().any(|key| key != "enabled") {
            return Err(refused("an override can only set \"enabled\""));
        }
        return match fields.get("enabled") {
            Some(Value::Bool(enabled)) => Ok(Entry::Override(Some(*enabled))),
            None => Ok(Entry::Override(None)),
            Some(_) => Err(refused("enabled must be a boolean")),
        };
    }
    if defined {
        return Err(refused(
            "the owner's mcp.json defines it; a project can only set its \"enabled\"",
        ));
    }
    let config = match validate_mcp_server_config(name, raw, &McpServerConfigOptions::default()) {
        McpServerConfigResult::Stdio(config) => config,
        McpServerConfigResult::Http(_) => {
            return Err(refused(
                "HTTP servers are for the owner's mcp.json (the p2 host would make the requests)",
            ));
        }
        McpServerConfigResult::Invalid(error) => return Err(error),
    };
    for (field, why) in [
        ("runOn", "a project's server runs where the project is"),
        ("shared", "a project's server runs for its session alone"),
        ("auth", "credentials are for the owner's mcp.json"),
        ("oauth", "credentials are for the owner's mcp.json"),
    ] {
        if raw.get(field).is_some() {
            return Err(refused(&format!(
                "\"{field}\" is for the owner's mcp.json: {why}"
            )));
        }
    }
    Ok(if config["enabled"] == false {
        Entry::Disabled
    } else {
        Entry::Stdio(config)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::mcp::config::parse_owner_config;

    #[test]
    fn projects_can_toggle_owner_servers_and_only_add_location_stdio() -> Result<(), String> {
        let owner = parse_owner_config(
            r#"{"mcpServers":{"my-docs":{"url":"https://example.test/mcp"},"off":{"command":"x","enabled":false}}}"#,
        )?;
        let project = parse_project_config(
            r#"{"mcpServers":{
                 "off":{"enabled":true},"my-docs":{"enabled":false},
                 "lint":{"command":"lint","env":{"TOKEN":"${SECRET}"}},
                 "remote":{"url":"https://example.test/mcp"},
                 "placed":{"command":"x","runOn":"host"},
                 "shared":{"command":"x","shared":true},
                 "auth":{"command":"x","auth":{"provider":"radius"}},
                 "sign":{"command":"x","oauth":{"clientId":"c"}},
                 "my_docs":{"command":"x"}
            }}"#,
            &owner,
        );
        assert_eq!(project.servers.len(), 1);
        assert_eq!(project.servers[0].name, "lint");
        assert_eq!(project.servers[0].config["env"]["TOKEN"], "${SECRET}");
        assert_eq!(
            project.enabled,
            IndexMap::from([("off".into(), true), ("my-docs".into(), false)])
        );
        assert_eq!(project.errors.len(), 6);
        assert!(
            project
                .errors
                .iter()
                .all(|error| error.starts_with(".pi/mcp.json: "))
        );
        Ok(())
    }
}

//! The owner's MCP configuration, `<P2_DATA_DIR>/mcp.json` (a trust boundary).
//!
//! The shared `mcpServers` shape is checked by pi-mcp; p2's own fields (`shared`, `runOn`) are
//! checked here. Invalid entries are skipped and reported; disabled ones are kept, because a
//! project may turn them on where it applies.

use pi_rs_mcp::{
    McpServerConfigOptions, McpServerConfigResult, mcp_namespace, validate_mcp_server_config,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::LazyLock;

/// Where a stdio server runs: where the conversation works, on the p2 host, or on an environment of `envs.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunOn {
    Location,
    Host,
    Env(String),
}

/// One server of the owner's configuration, validated.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum OwnerServer {
    Stdio {
        name: String,
        config: Value,
        enabled: bool,
        /// One process for every session (`shared`), else one per session and location.
        shared: bool,
        run_on: RunOn,
    },
    Http {
        name: String,
        config: Value,
        enabled: bool,
        /// One connection for every session (the default), else one per session.
        shared: bool,
    },
}

/// The owner's servers and why entries were skipped.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OwnerConfig {
    pub servers: Vec<OwnerServer>,
    pub errors: Vec<String>,
}

/// The `mcpServers` entries of `text`, a configuration file named `file` (comments allowed).
pub fn server_entries(text: &str, file: &str) -> Result<Map<String, Value>, String> {
    let value: Value = serde_json::from_str(&strip_json_comments(strip_bom(text)))
        .map_err(|_| format!("Invalid {file} syntax"))?;
    if !value.is_object() {
        return Err(format!("{file} must be an object"));
    }
    let entries = value
        .get("mcpServers")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    match entries {
        Value::Object(entries) => Ok(entries),
        _ => Err(format!("{file}: \"mcpServers\" must be an object")),
    }
}

/// The owner's configuration in `text` (`{ "mcpServers": { name: entry } }`, comments allowed).
pub fn parse_owner_config(text: &str) -> Result<OwnerConfig, String> {
    let mut config = OwnerConfig::default();
    let mut namespaces: Vec<(String, String)> = Vec::new();
    for (name, raw) in server_entries(text, "mcp.json")? {
        match owner_server(&name, &raw) {
            Err(error) => config.errors.push(error),
            Ok(server) => {
                // Tool names and code mode namespaces replace `-` by `_`: both spellings would collide.
                let namespace = mcp_namespace(&name);
                if let Some((_, taken)) = namespaces.iter().find(|(other, _)| other == &namespace) {
                    config.errors.push(format!(
                        "server \"{name}\": its tools would be named like those of \"{taken}\""
                    ));
                    continue;
                }
                namespaces.push((namespace, name.clone()));
                config.servers.push(server);
            }
        }
    }
    Ok(config)
}

/// One entry of the configuration, checked.
fn owner_server(name: &str, raw: &Value) -> Result<OwnerServer, String> {
    let invalid = |error: &str| format!("server \"{name}\": {error}");
    let result = validate_mcp_server_config(name, raw, &McpServerConfigOptions::default());
    let (config, kind) = match result {
        McpServerConfigResult::Invalid(error) => return Err(error),
        McpServerConfigResult::Stdio(config) => (config, "stdio"),
        McpServerConfigResult::Http(config) => (config, "http"),
    };
    // The entry keeps the fields pi-mcp does not know (p2's `shared` and `runOn` among them).
    let shared = config.get("shared").cloned();
    let run_on = config.get("runOn").cloned();
    if shared.as_ref().is_some_and(|shared| !shared.is_boolean()) {
        return Err(invalid("shared must be a boolean"));
    }
    let enabled = config.get("enabled").and_then(Value::as_bool) != Some(false);
    let shared = shared.and_then(|shared| shared.as_bool());
    if kind == "http" {
        if run_on.is_some() {
            return Err(invalid("runOn is for stdio servers"));
        }
        return Ok(OwnerServer::Http {
            name: name.to_owned(),
            config,
            enabled,
            shared: shared.unwrap_or(true),
        });
    }
    let Some(placement) = run_on_of(run_on.as_ref()) else {
        return Err(invalid(
            "runOn must be \"location\", \"host\" or { \"env\": \"<name in envs.json>\" }",
        ));
    };
    if shared == Some(true) && placement == Placement::Set(RunOn::Location) {
        return Err(invalid(
            "a shared server cannot run where each conversation works (use runOn \"host\" or an env)",
        ));
    }
    let run_on = match placement {
        Placement::Set(run_on) => run_on,
        // The default: a shared server runs on the host, the others where the conversation works.
        Placement::Absent if shared == Some(true) => RunOn::Host,
        Placement::Absent => RunOn::Location,
    };
    Ok(OwnerServer::Stdio {
        name: name.to_owned(),
        config,
        enabled,
        shared: shared.unwrap_or(false),
        run_on,
    })
}

/// `runOn` as configured: the default applies, the value is set, or it is invalid.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Placement {
    Absent,
    Set(RunOn),
}

fn run_on_of(value: Option<&Value>) -> Option<Placement> {
    match value {
        None => Some(Placement::Absent),
        Some(Value::String(text)) if text == "location" => Some(Placement::Set(RunOn::Location)),
        Some(Value::String(text)) if text == "host" => Some(Placement::Set(RunOn::Host)),
        Some(Value::Object(fields)) if fields.len() == 1 => fields
            .get("env")
            .and_then(Value::as_str)
            .filter(|env| !env.is_empty())
            .map(|env| Placement::Set(RunOn::Env(env.to_owned()))),
        Some(_) => None,
    }
}

/// Whether `value` is a JSON object.
pub fn is_record(value: &Value) -> bool {
    value.is_object()
}

/// `value` with every `${NAME}` replaced by the p2 host's environment variable (the owner's
/// configuration only); an unset variable is an error, naming it.
pub fn expand_host_variables(value: &str) -> Result<String, String> {
    static VARIABLE: LazyLock<Regex> = LazyLock::new(|| {
        // INFALLIBLE: the pattern is a constant.
        Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("valid static variable pattern")
    });
    let mut expanded = String::with_capacity(value.len());
    let mut last = 0;
    for found in VARIABLE.captures_iter(value) {
        let whole = found.get(0).expect("the whole match");
        let name = found.get(1).expect("the name").as_str();
        let variable =
            std::env::var(name).map_err(|_| format!("environment variable {name} is not set"))?;
        expanded.push_str(&value[last..whole.start()]);
        expanded.push_str(&variable);
        last = whole.end();
    }
    expanded.push_str(&value[last..]);
    Ok(expanded)
}

/// The text without its byte order mark.
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// The JSON with its comments and trailing commas removed, as the configuration files allow.
fn strip_json_comments(text: &str) -> String {
    static COMMENT: LazyLock<Regex> = LazyLock::new(|| {
        // INFALLIBLE: the pattern is a constant.
        Regex::new(r#""(?:\\.|[^"\\])*"|//[^\n]*"#).expect("valid static comment pattern")
    });
    static TRAILING: LazyLock<Regex> = LazyLock::new(|| {
        // INFALLIBLE: the pattern is a constant.
        Regex::new(r#""(?:\\.|[^"\\])*"|,(\s*[}\]])"#).expect("valid static trailing comma pattern")
    });
    let without_comments = keep_or_drop(text, &COMMENT, false);
    keep_or_drop(&without_comments, &TRAILING, true)
}

/// The text with every match of `pattern` kept (when `keep_strings`) or dropped.
///
/// A match that starts with a quote is a string: it is always kept. The other match of the
/// trailing comma pattern is a comma with the bracket after it: the bracket is kept, the comma
/// is not.
fn keep_or_drop(text: &str, pattern: &Regex, keep_strings: bool) -> String {
    let mut result = String::with_capacity(text.len());
    let mut last = 0;
    for found in pattern.captures_iter(text) {
        let whole = found.get(0).expect("the whole match");
        result.push_str(&text[last..whole.start()]);
        if whole.as_str().starts_with('"') {
            result.push_str(whole.as_str());
        } else if keep_strings {
            // The comma and the bracket after it: the bracket stays.
            let tail = found.get(1).map(|tail| tail.as_str()).unwrap_or_default();
            result.push_str(tail);
        }
        last = whole.end();
    }
    result.push_str(&text[last..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The owner servers of a configuration, by name.
    fn servers(config: &OwnerConfig) -> Vec<&str> {
        config
            .servers
            .iter()
            .map(|server| match server {
                OwnerServer::Stdio { name, .. } | OwnerServer::Http { name, .. } => name.as_str(),
            })
            .collect()
    }

    #[test]
    fn a_stdio_server_works_where_the_conversation_works() {
        let config = parse_owner_config(
            r#"{"mcpServers":{"files":{"command":"mcp-files","args":["--root","."]}}}"#,
        )
        .expect("a configuration");
        assert!(config.errors.is_empty());
        assert_eq!(servers(&config), ["files"]);
        let OwnerServer::Stdio {
            enabled,
            shared,
            run_on,
            ..
        } = &config.servers[0]
        else {
            panic!("a stdio server");
        };
        assert!(enabled);
        assert!(!shared);
        assert_eq!(*run_on, RunOn::Location);
    }

    #[test]
    fn an_http_server_is_shared_by_default() {
        let config = parse_owner_config(
            r#"{"mcpServers":{"docs":{"url":"https://mcp.example/mcp","type":"http"}}}"#,
        )
        .expect("a configuration");
        let OwnerServer::Http {
            shared, enabled, ..
        } = &config.servers[0]
        else {
            panic!("an http server");
        };
        assert!(*shared);
        assert!(*enabled);
    }

    #[test]
    fn a_shared_server_runs_on_the_host() {
        let config =
            parse_owner_config(r#"{"mcpServers":{"files":{"command":"mcp-files","shared":true}}}"#)
                .expect("a configuration");
        let OwnerServer::Stdio { shared, run_on, .. } = &config.servers[0] else {
            panic!("a stdio server");
        };
        assert!(*shared);
        assert_eq!(*run_on, RunOn::Host);
    }

    #[test]
    fn a_server_may_run_in_an_environment() {
        let config = parse_owner_config(
            r#"{"mcpServers":{"files":{"command":"mcp-files","runOn":{"env":"dev"}}}}"#,
        )
        .expect("a configuration");
        let OwnerServer::Stdio { run_on, .. } = &config.servers[0] else {
            panic!("a stdio server");
        };
        assert_eq!(*run_on, RunOn::Env("dev".into()));
    }

    #[test]
    fn a_disabled_server_is_kept() {
        let config = parse_owner_config(
            r#"{"mcpServers":{"files":{"command":"mcp-files","enabled":false}}}"#,
        )
        .expect("a configuration");
        assert_eq!(servers(&config), ["files"]);
        let OwnerServer::Stdio { enabled, .. } = &config.servers[0] else {
            panic!("a stdio server");
        };
        assert!(!enabled);
    }

    #[test]
    fn an_invalid_entry_is_skipped_and_reported() {
        let config = parse_owner_config(
            r#"{"mcpServers":{"files":{"command":"mcp-files","shared":"yes"},"broken":{"url":"not a url"}}}"#,
        )
        .expect("a configuration");
        assert!(config.servers.is_empty());
        assert_eq!(config.errors.len(), 2);
        assert!(
            config.errors[0].contains("shared must be a boolean"),
            "{:?}",
            config.errors
        );
    }

    #[test]
    fn a_run_on_that_is_not_a_run_on_is_refused() {
        let config = parse_owner_config(
            r#"{"mcpServers":{"files":{"command":"mcp-files","runOn":"elsewhere"}}}"#,
        )
        .expect("a configuration");
        assert!(config.servers.is_empty());
        assert!(
            config.errors[0].contains("runOn must be"),
            "{:?}",
            config.errors
        );
    }

    #[test]
    fn a_shared_server_may_not_run_where_each_conversation_works() {
        let config = parse_owner_config(
            r#"{"mcpServers":{"files":{"command":"mcp-files","shared":true,"runOn":"location"}}}"#,
        )
        .expect("a configuration");
        assert!(config.servers.is_empty());
        assert!(
            config.errors[0].contains("a shared server cannot run"),
            "{:?}",
            config.errors
        );
    }

    #[test]
    fn a_run_on_is_for_stdio_servers_only() {
        let config = parse_owner_config(
            r#"{"mcpServers":{"docs":{"url":"https://mcp.example/mcp","runOn":"host"}}}"#,
        )
        .expect("a configuration");
        assert!(config.servers.is_empty());
        assert!(
            config.errors[0].contains("runOn is for stdio servers"),
            "{:?}",
            config.errors
        );
    }

    #[test]
    fn two_names_that_share_a_namespace_are_refused() {
        let config = parse_owner_config(
            r#"{"mcpServers":{"a-b":{"command":"one"},"a_b":{"command":"two"}}}"#,
        )
        .expect("a configuration");
        assert_eq!(servers(&config), ["a-b"]);
        assert!(
            config.errors[0].contains("its tools would be named like"),
            "{:?}",
            config.errors
        );
    }

    #[test]
    fn a_configuration_may_have_comments_and_trailing_commas() {
        let config = parse_owner_config(
            "{\n  // the servers\n  \"mcpServers\": {\n    \"files\": {\"command\": \"mcp-files\",},\n  },\n}\n",
        )
        .expect("a configuration");
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        assert_eq!(servers(&config), ["files"]);
    }

    #[test]
    fn a_file_that_is_not_a_configuration_is_refused() {
        assert_eq!(
            server_entries("not json", "mcp.json").expect_err("invalid syntax"),
            "Invalid mcp.json syntax"
        );
        assert_eq!(
            server_entries("[]", "mcp.json").expect_err("not an object"),
            "mcp.json must be an object"
        );
        assert_eq!(
            server_entries(r#"{"mcpServers":[]}"#, "mcp.json").expect_err("not an object"),
            "mcp.json: \"mcpServers\" must be an object"
        );
    }

    #[test]
    fn a_host_variable_is_expanded() {
        let path = std::env::var("PATH").expect("a path in the environment");
        assert_eq!(
            expand_host_variables("dir-${PATH}").expect("an expanded variable"),
            format!("dir-{path}")
        );
        assert_eq!(
            expand_host_variables("${P2_MCP_TEST_UNSET}").expect_err("an unset variable"),
            "environment variable P2_MCP_TEST_UNSET is not set"
        );
    }
}

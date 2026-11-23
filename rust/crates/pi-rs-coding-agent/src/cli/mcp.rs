//! Standalone `pi mcp` subcommand: add, remove, list, and sign in to MCP servers outside a
//! session. Mirrors `packages/coding-agent/src/extensions/mcp/cli.ts`. Reads and writes
//! `mcp.json` in the agent directory and, for trusted projects, `.pi/mcp.json`.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use indexmap::IndexMap;
use pi_rs_mcp::{McpServerConfigOptions, McpServerConfigResult, validate_mcp_server_config};
use serde_json::{Map, Value};

use crate::config::{CONFIG_DIR_NAME, VERSION};
use crate::core::project_trust::ProjectTrustStore;
use crate::utils::text::strip_bom;

const GLOBAL_CONFIG_NAME: &str = "mcp.json";
const DEFAULT_LOGIN_TIMEOUT_SECONDS: u64 = 300;

const HELP: &str = r#"Usage:
  pi mcp add <server> [options] -- <command> [args...]
  pi mcp add <server> [options] --url <url>
  pi mcp remove <server> [-l]
  pi mcp list [--json]
  pi mcp login <server> [-l] [--timeout <seconds>]
  pi mcp logout <server> [-l]

Configure and check MCP servers and sign in to OAuth servers without starting a session.
Reads ~/.pi/agent/mcp.json and, in trusted projects, .pi/mcp.json.

Commands:
  add <server>            Add or replace a server in mcp.json
  remove <server>         Remove a server from mcp.json
  list                    Show state, tools, and errors (exits 1 on failure)
  login <server>          Sign in through the browser
  logout <server>         Delete the stored OAuth credentials

Options for add, remove, login, and logout:
  -l, --local             Use .pi/mcp.json in the current project instead of the global file

Options for add:
  --url <url>             Streamable HTTP server URL (instead of a command)
  --env <KEY=VALUE>       Environment variable for a stdio server (repeatable)
  --cwd <dir>             Working directory for a stdio server
  --header <KEY=VALUE>    HTTP header (repeatable)
  --bearer-token-env-var <NAME>
                          Send "Authorization: Bearer ${NAME}"
  --oauth-client-id <id>  Pre-registered OAuth client id
  --oauth-client-secret <secret>
                          OAuth client secret (may be ${NAME} or !command)
  --oauth-callback-port <port>
                          Fixed OAuth callback port
  --oauth-client-name <name>
                          Client name sent when registering with the OAuth server
  --oauth-client-registration <mode>
                          auto (default), cimd, or dcr
  --exposure <mode>       codemode (default), deferred, direct, or hidden
  --description <text>    What the server offers, shown in the system prompt

Other options:
  --json                  Print the list as JSON
  --timeout <seconds>     Time limit for the whole sign-in (default: 300)"#;

const HELP_HINT: &str = "Use \"pi mcp --help\" for usage.";

enum Kind {
    Flag,
    Value,
    List,
}

struct Parsed {
    positional: Vec<String>,
    values: IndexMap<String, String>,
    lists: IndexMap<String, Vec<String>>,
}

fn parse_options(
    args: &[String],
    known: &[(&str, Kind)],
    max_positionals: usize,
    error: &mut Vec<String>,
) -> Option<Parsed> {
    let mut positional = Vec::new();
    let mut values: IndexMap<String, String> = IndexMap::new();
    let mut lists: IndexMap<String, Vec<String>> = IndexMap::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let arg = if arg == "-l" { "--local" } else { arg };
        if arg == "--" || positional.len() >= max_positionals {
            positional.extend(
                args[index..]
                    .iter()
                    .skip(if arg == "--" { 1 } else { 0 })
                    .cloned(),
            );
            break;
        }
        if !arg.starts_with("--") {
            positional.push(arg.to_owned());
            index += 1;
            continue;
        }
        let name = &arg[2..];
        let kind = known
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, kind)| kind);
        let Some(kind) = kind else {
            error.push(format!("Unknown option {arg}.\n{HELP_HINT}"));
            return None;
        };
        match kind {
            Kind::Flag => {
                values.insert(name.to_owned(), "true".to_owned());
                index += 1;
            }
            Kind::Value | Kind::List => {
                let value = args.get(index + 1);
                let Some(value) = value else {
                    error.push(format!("--{name} needs a value."));
                    return None;
                };
                if matches!(kind, Kind::List) {
                    lists
                        .entry(name.to_owned())
                        .or_default()
                        .push(value.clone());
                } else {
                    values.insert(name.to_owned(), value.clone());
                }
                index += 2;
            }
        }
    }
    Some(Parsed {
        positional,
        values,
        lists,
    })
}

fn value_of(values: &IndexMap<String, String>, name: &str) -> Option<String> {
    values.get(name).cloned()
}

fn parse_pairs(
    option: &str,
    pairs: Option<&[String]>,
    error: &mut Vec<String>,
) -> Option<IndexMap<String, String>> {
    let mut record: IndexMap<String, String> = IndexMap::new();
    for pair in pairs.unwrap_or_default() {
        let Some(separator) = pair.find('=').filter(|index| *index > 0) else {
            error.push(format!("--{option} expects KEY=VALUE, got \"{pair}\"."));
            return None;
        };
        record.insert(
            pair[..separator].to_owned(),
            pair[separator + 1..].to_owned(),
        );
    }
    Some(record)
}

fn project_trusted(store: &ProjectTrustStore, cwd: &Path) -> bool {
    store.get(cwd).is_ok_and(|decision| decision == Some(true))
}

fn global_config(agent_dir: &Path) -> PathBuf {
    agent_dir.join(GLOBAL_CONFIG_NAME)
}

fn project_config(cwd: &Path) -> PathBuf {
    cwd.join(CONFIG_DIR_NAME).join(GLOBAL_CONFIG_NAME)
}

fn read_json_file(path: &Path) -> Result<Value, String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            serde_json::from_str(strip_bom(&text))
                .map_err(|error| format!("{}: {error}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(serde_json::json!({"mcpServers": {}}))
        }
        Err(error) => Err(format!("Could not read {}: {error}", path.display())),
    }
}

fn write_json_file(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(dir) = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty() && !dir.is_dir())
    {
        std::fs::DirBuilder::new()
            .recursive(true)
            .create(dir)
            .map_err(|error| format!("Could not create {}: {error}", dir.display()))?;
    }
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("Could not read {}: {error}", path.display())),
    };
    let indent = existing
        .as_deref()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let spaces = line.len() - line.trim_start_matches([' ', '\t']).len();
                (spaces > 0 && spaces < line.len()).then_some(&line[..spaces])
            })
        })
        .unwrap_or("  ");
    let mut bytes = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    serde::Serialize::serialize(
        value,
        &mut serde_json::Serializer::with_formatter(&mut bytes, formatter),
    )
    .map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    std::fs::write(path, bytes)
        .map_err(|error| format!("Could not write {}: {error}", path.display()))
}

fn mcp_servers(config: &Value) -> Option<&Map<String, Value>> {
    config.get("mcpServers").and_then(Value::as_object)
}

/// Load global and (when trusted) project MCP configuration, like upstream `loadMcpConfig`.
#[derive(Clone, Debug)]
pub(crate) struct LoadedConfig {
    pub(crate) servers: IndexMap<String, Value>,
    scopes: IndexMap<String, String>,
    overrides: IndexMap<String, PathBuf>,
    pub(crate) errors: Vec<String>,
}

pub(crate) fn load_config(agent_dir: &Path, cwd: &Path, project_trusted: bool) -> LoadedConfig {
    let mut servers = IndexMap::new();
    let mut scopes = IndexMap::new();
    let mut overrides = IndexMap::new();
    let mut errors = Vec::new();
    for (path, scope) in [
        (global_config(agent_dir), "global"),
        (project_config(cwd), "project"),
    ] {
        if scope == "project" && !project_trusted {
            continue;
        }
        let config = match read_json_file(&path) {
            Ok(config) => config,
            Err(message) => {
                errors.push(message);
                continue;
            }
        };
        if !config.is_object() || config.get("mcpServers").is_some_and(|v| !v.is_object()) {
            errors.push(format!(
                "{}: expected an object with an \"mcpServers\" object",
                path.display()
            ));
            continue;
        }
        let servers_obj = match mcp_servers(&config) {
            Some(servers) => servers,
            None => continue,
        };
        for (name, value) in servers_obj {
            if scope == "project"
                && value.is_object()
                && !value
                    .as_object()
                    .is_some_and(|object| object.contains_key("command"))
                && !value
                    .as_object()
                    .is_some_and(|object| object.contains_key("url"))
                && !value
                    .as_object()
                    .is_some_and(|object| object.contains_key("type"))
            {
                let base = match servers.get(name) {
                    Some(base) => base,
                    None => {
                        errors.push(format!(
                            "{path}: server \"{name}\" needs \"command\" or \"url\", or a global server to override",
                            path = path.display(),
                            name = name
                        ));
                        continue;
                    }
                };
                let extra: Vec<String> = value
                    .as_object()
                    .map(|object| {
                        object
                            .keys()
                            .filter(|key| {
                                !["enabled", "exposure", "toolExposure"].contains(&key.as_str())
                            })
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                if !extra.is_empty() {
                    errors.push(format!(
                        "{path}: server \"{name}\": an override can only set enabled, exposure, toolExposure",
                        path = path.display(),
                        name = name
                    ));
                    continue;
                }
                let merged = merge_json(base, value);
                match validate_mcp_server_config(name, &merged, &McpServerConfigOptions::default())
                {
                    McpServerConfigResult::Invalid(message) => {
                        errors.push(format!(
                            "{path}: {message}",
                            path = path.display(),
                            message = message
                        ));
                    }
                    result => {
                        servers.insert(name.to_owned(), into_config_value(&result));
                        overrides.insert(name.to_owned(), path.clone());
                    }
                }
                continue;
            }
            match validate_mcp_server_config(name, value, &McpServerConfigOptions::default()) {
                McpServerConfigResult::Invalid(message) => {
                    errors.push(format!(
                        "{path}: {message}",
                        path = path.display(),
                        message = message
                    ));
                }
                result => {
                    let clash = servers.iter().find(|(other, _)| {
                        other.as_str() != name.as_str()
                            && mcp_namespace(other) == mcp_namespace(name)
                    });
                    if let Some((other, _)) = clash {
                        errors.push(format!(
                            "{path}: server \"{name}\" conflicts with \"{other}\"",
                            path = path.display(),
                            name = name,
                            other = other
                        ));
                        continue;
                    }
                    if scope == "project"
                        && let McpServerConfigResult::Http(config) = &result
                        && config.get("auth").is_some()
                    {
                        errors.push(format!(
                                    "{path}: server \"{name}\": auth is only allowed in the global mcp.json",
                                    path = path.display(),
                                    name = name
                                ));
                        continue;
                    }
                    servers.insert(name.to_owned(), into_config_value(&result));
                    scopes.insert(name.to_owned(), scope.to_owned());
                }
            }
        }
    }
    LoadedConfig {
        servers,
        scopes,
        overrides,
        errors,
    }
}

use pi_rs_mcp::mcp_namespace;

fn merge_json(base: &Value, patch: &Value) -> Value {
    let mut merged = base.clone();
    if let (Some(obj), Some(patch)) = (merged.as_object_mut(), patch.as_object()) {
        for (key, value) in patch {
            obj.insert(key.clone(), value.clone());
        }
    }
    merged
}

fn into_config_value(result: &McpServerConfigResult) -> Value {
    match result {
        McpServerConfigResult::Stdio(value) | McpServerConfigResult::Http(value) => value.clone(),
        McpServerConfigResult::Invalid(_) => Value::Null,
    }
}

fn add_server(
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    name: &str,
    command: &[String],
    parsed: &Parsed,
    error: &mut Vec<String>,
) -> u8 {
    let values = &parsed.values;
    let lists = &parsed.lists;
    let usage = format!(
        "Usage: pi mcp add <server> [options] (--url <url> | -- <command> [args...])\n{HELP_HINT}"
    );
    if name.is_empty() || (value_of(values, "url").is_some()) == !command.is_empty() {
        error.push(usage);
        return 1;
    }
    let http_only: &[&str] = &[
        "header",
        "bearer-token-env-var",
        "oauth-client-id",
        "oauth-client-secret",
        "oauth-callback-port",
        "oauth-client-name",
        "oauth-client-registration",
    ];
    let stdio_only: &[&str] = &["env", "cwd"];
    let url = value_of(values, "url");
    let misplaced = if url.is_none() { http_only } else { stdio_only }
        .iter()
        .find(|option| {
            let option = *option;
            values.iter().any(|(key, _)| key == option)
                || lists.iter().any(|(key, _)| key == option)
        });
    if let Some(option) = misplaced {
        error.push(format!(
            "--{option} only applies to {} servers.",
            if url.is_none() {
                "HTTP (--url)"
            } else {
                "stdio"
            }
        ));
        return 1;
    }
    let mut config = Map::new();
    if let Some(url) = url {
        if let Some(headers) = parse_pairs("header", lists.get("header").map(Vec::as_slice), error)
        {
            if !headers.is_empty() {
                config.insert(
                    "headers".to_owned(),
                    Value::Object(
                        headers
                            .into_iter()
                            .map(|(k, v)| (k, Value::String(v)))
                            .collect::<Map<_, _>>(),
                    ),
                );
            }
        } else {
            return 1;
        }
        if let Some(bearer) = &value_of(values, "bearer-token-env-var") {
            let header = format!("Bearer ${{{bearer}}}");
            if let Some(object) = config.get_mut("headers").and_then(Value::as_object_mut) {
                object.insert("Authorization".to_owned(), Value::String(header));
            } else {
                let mut headers = Map::new();
                headers.insert("Authorization".to_owned(), Value::String(header));
                config.insert("headers".to_owned(), Value::Object(headers));
            }
        }
        let mut oauth = Map::new();
        if let Some(id) = value_of(values, "oauth-client-id") {
            oauth.insert("clientId".to_owned(), Value::String(id));
        }
        if let Some(secret) = value_of(values, "oauth-client-secret") {
            oauth.insert("clientSecret".to_owned(), Value::String(secret));
        }
        if let Some(port) = value_of(values, "oauth-callback-port") {
            let number = port.parse::<u64>().unwrap_or(0).into();
            oauth.insert("callbackPort".to_owned(), Value::Number(number));
        }
        if let Some(client_name) = value_of(values, "oauth-client-name") {
            oauth.insert("clientName".to_owned(), Value::String(client_name));
        }
        if let Some(registration) = value_of(values, "oauth-client-registration") {
            oauth.insert("clientRegistration".to_owned(), Value::String(registration));
        }
        if !oauth.is_empty() {
            config.insert("oauth".to_owned(), Value::Object(oauth));
        }
        config.insert("url".to_owned(), Value::String(url));
    } else {
        let mut args_vec = Vec::new();
        for arg in command.iter().skip(1) {
            args_vec.push(Value::String(arg.clone()));
        }
        config.insert("command".to_owned(), Value::String(command[0].clone()));
        if !args_vec.is_empty() {
            config.insert("args".to_owned(), Value::Array(args_vec));
        }
        if let Some(env) = parse_pairs("env", lists.get("env").map(Vec::as_slice), error) {
            if !env.is_empty() {
                config.insert(
                    "env".to_owned(),
                    Value::Object(
                        env.into_iter()
                            .map(|(k, v)| (k, Value::String(v)))
                            .collect::<Map<_, _>>(),
                    ),
                );
            }
        } else {
            return 1;
        }
        if let Some(cwd) = value_of(values, "cwd") {
            config.insert("cwd".to_owned(), Value::String(cwd));
        }
    }
    if let Some(exposure) = value_of(values, "exposure") {
        match exposure.as_str() {
            "codemode" | "deferred" | "direct" | "hidden" => {
                config.insert("exposure".to_owned(), Value::String(exposure));
            }
            _ => {
                error.push(format!(
                    "Invalid exposure \"{exposure}\". Valid values: codemode, deferred, direct, hidden"
                ));
                return 1;
            }
        }
    }
    if let Some(description) = value_of(values, "description") {
        config.insert("description".to_owned(), Value::String(description));
    }
    let config = Value::Object(config);
    let may_need_sign_in = matches!(
        &config,
        Value::Object(entry)
            if entry.get("url").is_some()
                && entry
                    .get("headers")
                    .and_then(Value::as_object)
                    .is_none_or(|headers| !headers.contains_key("authorization") && !headers.contains_key("Authorization"))
    );
    if let McpServerConfigResult::Invalid(message) =
        validate_mcp_server_config(name, &config, &McpServerConfigOptions::default())
    {
        error.push(message);
        return 1;
    }
    let project = values.contains_key("local");
    let path = if project {
        project_config(cwd)
    } else {
        global_config(agent_dir)
    };
    let scope = if project { "project" } else { "global" };
    let mut parsed = match read_json_file(&path) {
        Ok(Value::Object(map)) => Value::Object(map),
        Ok(_) => {
            error.push(format!(
                "Could not update {}: expected a config object",
                path.display()
            ));
            return 1;
        }
        Err(error_message) => {
            error.push(format!(
                "Could not update {}: {error_message}",
                path.display()
            ));
            return 1;
        }
    };
    let Some(object) = parsed.as_object_mut() else {
        error.push("Expected a config object".into());
        return 1;
    };
    let Some(servers) = object
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
    else {
        error.push("Expected mcpServers to be an object".into());
        return 1;
    };
    let replaced = servers.insert(name.to_owned(), config).is_some();
    if let Err(error_message) = write_json_file(&path, &parsed) {
        error.push(format!(
            "Could not update {}: {error_message}",
            path.display()
        ));
        return 1;
    }
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    let _ = writeln!(
        handle,
        "{replaced} {scope} MCP server \"{name}\" in {path}.",
        replaced = if replaced { "Replaced" } else { "Added" },
        scope = scope,
        name = name,
        path = path.display()
    );
    if project && !project_trusted {
        let _ = writeln!(
            handle,
            "The project is not trusted, so {} is ignored until you start pi in the project and trust it.",
            path.display()
        );
    }
    if may_need_sign_in {
        let _ = writeln!(
            handle,
            "Check it with: pi mcp list. If it requires sign-in: pi mcp login {name}",
            name = name
        );
    }
    drop(handle);
    0
}

fn remove_server(
    agent_dir: &Path,
    cwd: &Path,
    name: &str,
    values: &IndexMap<String, String>,
    error: &mut Vec<String>,
) -> u8 {
    let project = values.contains_key("local");
    let path = if project {
        project_config(cwd)
    } else {
        global_config(agent_dir)
    };
    let scope = if project { "project" } else { "global" };
    let mut parsed = match read_json_file(&path) {
        Ok(Value::Object(map)) => Value::Object(map),
        Ok(_) => {
            error.push(format!(
                "Could not update {}: expected a config object",
                path.display()
            ));
            return 1;
        }
        Err(error_message) => {
            error.push(format!(
                "Could not update {}: {error_message}",
                path.display()
            ));
            return 1;
        }
    };
    if parsed
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .and_then(|servers| servers.shift_remove(name))
        .is_some()
    {
        if let Err(error_message) = write_json_file(&path, &parsed) {
            error.push(format!(
                "Could not update {}: {error_message}",
                path.display()
            ));
            return 1;
        }
        let stdout = io::stdout();
        let mut handle = stdout.lock();
        let _ = writeln!(
            handle,
            "Removed {scope} MCP server \"{name}\" from {}.",
            path.display()
        );
        drop(handle);
        return 0;
    }
    let loaded = load_config(agent_dir, cwd, true);
    let note = match loaded.scopes.get(name).map(String::as_str) {
        Some("project") if !project => format!(
            " It is defined in {}; use --local.",
            project_config(cwd).display()
        ),
        Some("global") if project => format!(
            " It is defined in {}; omit --local.",
            global_config(agent_dir).display()
        ),
        _ => String::new(),
    };
    error.push(format!(
        "No {scope} MCP server named \"{name}\" in {}.{note}",
        path.display()
    ));
    1
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ServerReport {
    name: String,
    scope: String,
    source: PathBuf,
    #[serde(rename = "override", skip_serializing_if = "Option::is_none")]
    override_path: Option<PathBuf>,
    enabled: bool,
    exposure: String,
    transport: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_provider: Option<String>,
    state: &'static str,
    tools: Vec<String>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    tool_exposure: IndexMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resources: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn scope_of<'a>(loaded: &'a LoadedConfig, name: &str) -> &'a str {
    loaded
        .scopes
        .get(name)
        .map(String::as_str)
        .unwrap_or("global")
}

pub(crate) async fn discover(
    agent_dir: &Path,
    name: &str,
    server: &Value,
    cwd: &Path,
) -> Result<(pi_rs_mcp::McpClient, Vec<pi_rs_mcp::Tool>), String> {
    discover_with_challenge(agent_dir, name, server, cwd, Arc::new(|_| {})).await
}

async fn discover_with_challenge(
    agent_dir: &Path,
    name: &str,
    server: &Value,
    cwd: &Path,
    on_challenge: Arc<dyn Fn(&pi_rs_mcp::oauth::OAuthChallenge) + Send + Sync>,
) -> Result<(pi_rs_mcp::McpClient, Vec<pi_rs_mcp::Tool>), String> {
    let transport: Arc<dyn pi_rs_mcp::McpTransport> =
        match validate_mcp_server_config(name, server, &McpServerConfigOptions::default()) {
            McpServerConfigResult::Stdio(server) => {
                let transport = pi_rs_mcp::StdioTransport::new(pi_rs_mcp::StdioTransportOptions {
                    command: server
                        .get("command")
                        .and_then(Value::as_str)
                        .ok_or("Missing command")?
                        .to_owned(),
                    args: server
                        .get("args")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                    cwd: server
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(|path| cwd.join(path).to_string_lossy().into_owned()),
                    env: string_map(server.get("env")),
                    ..Default::default()
                });
                transport as Arc<dyn pi_rs_mcp::McpTransport>
            }
            McpServerConfigResult::Http(server) => {
                let url = server["url"].as_str().ok_or("Missing URL")?;
                let mut headers = string_map(server.get("headers")).unwrap_or_default();
                for value in headers.values_mut() {
                    *value =
                        crate::core::model_registry::expand_or_literal(value, &IndexMap::new());
                }
                let mut auth_provider: Option<Arc<dyn pi_rs_mcp::AuthProvider>> = None;
                if let Some(provider) = server["auth"]["provider"].as_str() {
                    let runtime = super::auth_command::Runtime::new(agent_dir, true, false)?;
                    if let Some(auth) = runtime
                        .get_auth(provider, None, &Default::default())
                        .await
                        .map_err(|e| e.to_string())?
                        && let Some(key) = auth.auth.api_key
                    {
                        headers.insert("Authorization".to_owned(), format!("Bearer {key}"));
                    }
                } else if !headers
                    .keys()
                    .any(|key| key.eq_ignore_ascii_case("authorization"))
                {
                    use pi_rs_mcp::oauth::{McpAuthProviderOptions, create_mcp_auth_provider};
                    let settings = crate::core::mcp_oauth::settings(name, &server["oauth"])?;
                    auth_provider = Some(create_mcp_auth_provider(McpAuthProviderOptions {
                        server_url: url.to_owned(),
                        store: Arc::new(crate::core::mcp_oauth::Store::new(agent_dir, name, url)?),
                        client_name: "pi".to_owned(),
                        client_metadata_documents: None,
                        settings: Arc::new(move || settings.clone()),
                        on_challenge,
                        redirect_url: None,
                    }));
                }
                let transport = pi_rs_mcp::StreamableHttpTransport::new(
                    pi_rs_mcp::StreamableHttpTransportOptions {
                        url: server
                            .get("url")
                            .and_then(Value::as_str)
                            .ok_or("Missing URL")?
                            .to_owned(),
                        headers: Some(headers),
                        auth_provider,
                        open_get_stream: Some(false),
                        ..Default::default()
                    },
                )
                .map_err(|error| error.to_string())?;
                transport as Arc<dyn pi_rs_mcp::McpTransport>
            }
            McpServerConfigResult::Invalid(message) => return Err(message),
        };
    let client = pi_rs_mcp::McpClient::new(pi_rs_mcp::McpClientOptions {
        name: "pi".to_owned(),
        version: VERSION.to_owned(),
        ..Default::default()
    });
    let result = async {
        let initialized = client.connect(transport).await?;
        if initialized.capabilities.tools.is_none() {
            return Ok::<_, pi_rs_mcp::McpClientError>(Vec::new());
        }
        client.list_tools(&Default::default()).await
    }
    .await;
    match result {
        Ok(tools) => Ok((client, tools)),
        Err(error) => {
            client.close().await;
            Err(error.to_string())
        }
    }
}

pub(crate) async fn inspect(
    agent_dir: &Path,
    name: &str,
    server: &Value,
    cwd: &Path,
) -> Result<Vec<String>, String> {
    let (client, tools) = discover(agent_dir, name, server, cwd).await?;
    client.close().await;
    Ok(tools.into_iter().map(|tool| tool.name).collect())
}

fn string_map(value: Option<&Value>) -> Option<IndexMap<String, String>> {
    let object = value.and_then(Value::as_object)?;
    Some(
        object
            .iter()
            .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
            .collect(),
    )
}

async fn run(argv: &[String], cwd: &Path, agent_dir: &Path) -> Option<u8> {
    let [head, rest @ ..] = argv else { return None };
    if head != "mcp" {
        return None;
    }
    let Some((name, args)) = rest.split_first() else {
        println!("{HELP}");
        return Some(0);
    };
    if name == "help"
        || rest
            .iter()
            .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!("{HELP}");
        return Some(0);
    }
    let store = ProjectTrustStore::new(agent_dir);
    let project_trusted = project_trusted(&store, cwd);
    let mut error = Vec::new();
    let code = match name.as_str() {
        "list" | "status" => {
            let Some(parsed) = parse_options(args, &[("json", Kind::Flag)], usize::MAX, &mut error)
            else {
                return Some(report_errors(&error));
            };
            if !parsed.positional.is_empty() {
                error.push(format!("Usage: pi mcp list [--json]\n{HELP_HINT}"));
                return Some(report_errors(&error));
            }
            list_servers(
                agent_dir,
                cwd,
                project_trusted,
                parsed.values.contains_key("json"),
            )
            .await
        }
        "add" => {
            let Some(parsed) = parse_options(
                args,
                &[
                    ("local", Kind::Flag),
                    ("url", Kind::Value),
                    ("env", Kind::List),
                    ("cwd", Kind::Value),
                    ("header", Kind::List),
                    ("bearer-token-env-var", Kind::Value),
                    ("oauth-client-id", Kind::Value),
                    ("oauth-client-secret", Kind::Value),
                    ("oauth-callback-port", Kind::Value),
                    ("oauth-client-name", Kind::Value),
                    ("oauth-client-registration", Kind::Value),
                    ("exposure", Kind::Value),
                    ("description", Kind::Value),
                ],
                2,
                &mut error,
            ) else {
                return Some(report_errors(&error));
            };
            let Some((name, command)) = parsed.positional.split_first() else {
                error.push(format!("Usage: pi mcp add <server> [options] (--url <url> | -- <command> [args...])\n{HELP_HINT}"));
                return Some(report_errors(&error));
            };
            add_server(
                agent_dir,
                cwd,
                project_trusted,
                name,
                command,
                &parsed,
                &mut error,
            )
        }
        "remove" => {
            let Some(parsed) =
                parse_options(args, &[("local", Kind::Flag)], usize::MAX, &mut error)
            else {
                return Some(report_errors(&error));
            };
            let [name] = parsed.positional.as_slice() else {
                error.push(format!("Usage: pi mcp remove <server> [-l]\n{HELP_HINT}"));
                return Some(report_errors(&error));
            };
            remove_server(agent_dir, cwd, name, &parsed.values, &mut error)
        }
        "login" | "auth" => {
            let Some(parsed) = parse_options(
                args,
                &[("local", Kind::Flag), ("timeout", Kind::Value)],
                usize::MAX,
                &mut error,
            ) else {
                return Some(report_errors(&error));
            };
            let [name] = parsed.positional.as_slice() else {
                error.push(format!(
                    "Usage: pi mcp login <server> [-l] [--timeout <seconds>]\n{HELP_HINT}"
                ));
                return Some(report_errors(&error));
            };
            login_server(
                agent_dir,
                cwd,
                project_trusted,
                name,
                &parsed.values,
                &mut error,
            )
            .await
        }
        "logout" => {
            let Some(parsed) =
                parse_options(args, &[("local", Kind::Flag)], usize::MAX, &mut error)
            else {
                return Some(report_errors(&error));
            };
            let [name] = parsed.positional.as_slice() else {
                error.push(format!("Usage: pi mcp logout <server> [-l]\n{HELP_HINT}"));
                return Some(report_errors(&error));
            };
            logout_server(
                agent_dir,
                cwd,
                project_trusted,
                name,
                &parsed.values,
                &mut error,
            )
            .await
        }
        _ => {
            error.push(format!("Unknown mcp command \"{name}\".\n{HELP_HINT}"));
            return Some(report_errors(&error));
        }
    };
    Some(if error.is_empty() {
        code
    } else {
        report_errors(&error)
    })
}

fn needs_auth(error: &str) -> bool {
    error.contains("401")
        || error.contains("403")
        || error.to_ascii_lowercase().contains("unauthorized")
        || error.contains("requires user interaction")
}

async fn list_servers(agent_dir: &Path, cwd: &Path, project_trusted: bool, json: bool) -> u8 {
    let loaded = load_config(agent_dir, cwd, project_trusted);
    let mut reports = Vec::new();
    for (name, server) in &loaded.servers {
        let exposure = server
            .get("exposure")
            .and_then(Value::as_str)
            .unwrap_or("codemode")
            .to_owned();
        let enabled = server
            .get("enabled")
            .is_none_or(|value| value.as_bool().unwrap_or(true));
        let transport = if server.get("url").is_some() {
            server
                .get("url")
                .and_then(Value::as_str)
                .map_or_else(|| "http".to_owned(), |url| url.to_owned())
        } else {
            let mut text = server
                .get("command")
                .and_then(Value::as_str)
                .map_or_else(|| "stdio".to_owned(), |command| command.to_owned());
            if let Some(args) = server.get("args").and_then(Value::as_array) {
                for arg in args {
                    if let Some(arg) = arg.as_str() {
                        text.push(' ');
                        text.push_str(arg);
                    }
                }
            }
            text
        };
        let scope = scope_of(&loaded, name);
        let mut report = ServerReport {
            name: name.clone(),
            scope: scope.to_owned(),
            source: if scope == "global" {
                global_config(agent_dir)
            } else {
                project_config(cwd)
            },
            override_path: loaded.overrides.get(name).cloned(),
            enabled,
            exposure,
            transport,
            auth_provider: server["auth"]["provider"].as_str().map(str::to_owned),
            state: "disabled",
            tools: Vec::new(),
            tool_exposure: IndexMap::new(),
            resources: None,
            error: None,
        };
        if enabled {
            match discover(agent_dir, name, server, cwd).await {
                Ok((client, tools)) => {
                    report.state = "connected";
                    report.resources = client
                        .server_capabilities()
                        .and_then(|c| c.resources.map(|_| true));
                    for tool in tools {
                        if let Some(value) = server["toolExposure"][&tool.name].as_str()
                            && value != report.exposure
                        {
                            report
                                .tool_exposure
                                .insert(tool.name.clone(), value.to_owned());
                        }
                        report.tools.push(tool.name);
                    }
                    client.close().await;
                }
                Err(error) => {
                    report.state = if needs_auth(&error) {
                        "needs-auth"
                    } else {
                        "failed"
                    };
                    report.error = Some(error);
                }
            }
        }
        reports.push(report);
    }
    let failed =
        !loaded.errors.is_empty() || reports.iter().any(|r| r.enabled && r.state != "connected");
    let path = project_config(cwd);
    let note=(!project_trusted && path.is_file()).then(||format!("{} is ignored because the project is not trusted. Start pi in the project to trust it.",path.display()));
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    if json {
        let mut root = serde_json::json!({"servers":reports,"errors":loaded.errors});
        if let Some(note) = note {
            root["note"] = Value::String(note);
        }
        if serde_json::to_writer_pretty(&mut handle, &root).is_err() || writeln!(handle).is_err() {
            return 1;
        }
    } else {
        if reports.is_empty() && loaded.errors.is_empty() {
            let _ = writeln!(
                handle,
                "No MCP servers configured. Add them to {} or .pi/mcp.json.",
                global_config(agent_dir).display()
            );
        }
        for report in &reports {
            let state_text = match report.state {
                "connected" => format!(
                    "connected, {} tool{}",
                    report.tools.len(),
                    if report.tools.len() == 1 { "" } else { "s" }
                ),
                "needs-auth" => "needs sign-in".to_owned(),
                _ => report.state.to_owned(),
            };
            let _ = writeln!(
                handle,
                "{}: {} ({}, {})",
                report.name, state_text, report.exposure, report.scope
            );
            let _ = writeln!(handle, "  {}", report.transport);
            if let Some(path) = &report.override_path {
                let _ = writeln!(handle, "  project override: {}", path.display());
            }
            if report.state == "needs-auth" {
                let hint = report.auth_provider.as_ref().map_or_else(
                    || format!("pi mcp login {}", report.name),
                    |p| format!("/login {p} in pi"),
                );
                let _ = writeln!(handle, "  sign in with: {hint}");
            }
            if !report.tools.is_empty() {
                let tools: Vec<_> = report
                    .tools
                    .iter()
                    .map(|name| {
                        report
                            .tool_exposure
                            .get(name)
                            .map_or_else(|| name.clone(), |e| format!("{name} [{e}]"))
                    })
                    .collect();
                let _ = writeln!(handle, "  tools: {}", tools.join(", "));
            }
            if report.resources == Some(true) {
                let _ = writeln!(handle, "  offers resources");
            }
            if let Some(message) = &report.error {
                for line in message.split('\n') {
                    let _ = writeln!(handle, "  {line}");
                }
            }
        }
        for error_message in &loaded.errors {
            let _ = writeln!(handle, "config error: {error_message}");
        }
        if let Some(note) = note {
            let _ = writeln!(handle, "{note}");
        }
    }
    drop(handle);
    if failed { 1 } else { 0 }
}

pub(crate) fn oauth_url(entry: &Value) -> Option<&str> {
    if entry["auth"]["provider"].is_string()
        || entry["headers"].as_object().is_some_and(|headers| {
            headers
                .keys()
                .any(|key| key.eq_ignore_ascii_case("authorization"))
        })
    {
        return None;
    }
    entry["url"].as_str()
}

async fn login_server(
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    name: &str,
    values: &IndexMap<String, String>,
    error: &mut Vec<String>,
) -> u8 {
    let loaded = load_config(agent_dir, cwd, project_trusted);
    let entry = match loaded.servers.get(name) {
        Some(entry) => entry,
        None => {
            let names: Vec<_> = loaded.servers.keys().cloned().collect();
            let suggestion = names
                .iter()
                .find(|candidate| edit_distance(candidate, name) <= 2);
            error.push(format!(
                "No MCP server named \"{name}\".{}{}. Configured: {}.",
                suggestion
                    .map(|candidate| format!(" Did you mean \"{candidate}\"?"))
                    .unwrap_or_default(),
                if project_trusted {
                    ""
                } else {
                    " The project mcp.json may be hidden until trusted."
                },
                names.join(", ")
            ));
            return 1;
        }
    };
    if values.contains_key("local") && scope_of(&loaded, name) != "project" {
        error.push(format!(
            "MCP server \"{name}\" is defined in the global mcp.json, not in the project; omit --local."
        ));
        return 1;
    }
    if let Some(provider) = entry
        .get("auth")
        .and_then(Value::as_object)
        .and_then(|auth| auth.get("provider"))
        .and_then(Value::as_str)
    {
        error.push(format!(
            "MCP server \"{name}\" uses the credentials of provider \"{provider}\". Run \"pi auth login {provider}\"."
        ));
        return 1;
    }
    let url = match oauth_url(entry) {
        Some(url) => url,
        None => {
            error.push(format!(
                "MCP server \"{name}\" does not use OAuth. Only HTTP servers without an Authorization header do."
            ));
            return 1;
        }
    };
    let timeout = match value_of(values, "timeout") {
        Some(value) => match value.parse::<u64>() {
            Ok(timeout) if timeout > 0 => timeout,
            _ => {
                error.push("--timeout must be a positive number of seconds.".to_owned());
                return 1;
            }
        },
        None => DEFAULT_LOGIN_TIMEOUT_SECONDS,
    };
    let challenge = Arc::new(std::sync::OnceLock::new());
    let captured = challenge.clone();
    let probe = discover_with_challenge(
        agent_dir,
        name,
        entry,
        cwd,
        Arc::new(move |value| {
            let _ = captured.set(value.clone());
        }),
    )
    .await;
    match probe {
        Ok((client, tools)) => {
            client.close().await;
            println!(
                "Already signed in to MCP server \"{name}\" ({} tools).",
                tools.len()
            );
            return 0;
        }
        Err(message) if !needs_auth(&message) => {
            error.push(format!(
                "MCP server \"{name}\" failed to connect: {message}"
            ));
            return 1;
        }
        Err(_) => {}
    }
    let signed_in = async {
        crate::core::mcp_oauth::login(
            agent_dir,
            name,
            url,
            &entry["oauth"],
            timeout,
            challenge.get().cloned(),
        )
        .await?;
        inspect(agent_dir, name, entry, cwd)
            .await
            .map(|tools| tools.len())
    }
    .await;
    match signed_in {
        Ok(tools) => {
            let stdout = io::stdout();
            let mut handle = stdout.lock();
            let _ = writeln!(
                handle,
                "Signed in to MCP server \"{name}\" ({tools} tools)."
            );
            drop(handle);
            0
        }
        Err(message) => {
            error.push(format!(
                "Sign-in to MCP server \"{name}\" failed: {message}"
            ));
            1
        }
    }
}

async fn logout_server(
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    name: &str,
    values: &IndexMap<String, String>,
    error: &mut Vec<String>,
) -> u8 {
    let loaded = load_config(agent_dir, cwd, project_trusted);
    let entry = match loaded.servers.get(name) {
        Some(entry) => entry,
        None => {
            error.push(format!("No MCP server named \"{name}\"."));
            return 1;
        }
    };
    if values.contains_key("local") && scope_of(&loaded, name) != "project" {
        error.push(format!(
            "MCP server \"{name}\" is defined in the global mcp.json, not in the project; omit --local."
        ));
        return 1;
    }
    if oauth_url(entry).is_none() {
        error.push(format!(
            "MCP server \"{name}\" does not use OAuth. Only HTTP servers without an Authorization header do."
        ));
        return 1;
    }
    let removed = match crate::core::mcp_oauth::Store::new(
        agent_dir,
        name,
        entry["url"].as_str().unwrap_or_default(),
    )
    .and_then(|store| store.remove())
    {
        Ok(removed) => removed,
        Err(message) => {
            error.push(message);
            return 1;
        }
    };
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    if removed {
        let _ = writeln!(handle, "Signed out of MCP server \"{name}\".");
    } else {
        let _ = writeln!(handle, "No stored credentials for MCP server \"{name}\".");
    }
    drop(handle);
    0
}

fn report_errors(error: &[String]) -> u8 {
    for message in error {
        eprintln!("{message}");
    }
    1
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<_> = b.encode_utf16().collect();
    let mut previous: Vec<_> = (0..=b.len()).collect();
    for (i, a_char) in a.encode_utf16().enumerate() {
        let mut current = vec![i + 1];
        for (j, b_char) in b.iter().enumerate() {
            current.push(
                (previous[j + 1] + 1)
                    .min(current[j] + 1)
                    .min(previous[j] + usize::from(a_char != *b_char)),
            );
        }
        previous = current;
    }
    previous[b.len()]
}

/// Handles `mcp` arguments (without the executable name) and returns their exit code.
/// Returns `None` if the first argument is not `mcp`.
pub async fn run_command(argv: &[String], cwd: &Path, agent_dir: &Path) -> Option<u8> {
    run(argv, cwd, agent_dir).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_options_flags_values_and_lists() -> Result<(), String> {
        let mut error = Vec::new();
        let parsed = parse_options(
            &[
                "--url".to_owned(),
                "https://example.com/mcp".to_owned(),
                "--env".to_owned(),
                "A=1".to_owned(),
                "--env".to_owned(),
                "B=2".to_owned(),
                "--header".to_owned(),
                "X=Y".to_owned(),
                "fs".to_owned(),
                "cp".to_owned(),
                "/tmp".to_owned(),
            ],
            &[
                ("url", Kind::Value),
                ("env", Kind::List),
                ("header", Kind::List),
            ],
            2,
            &mut error,
        )
        .ok_or_else(|| error.join("\n"))?;
        assert_eq!(parsed.positional, ["fs", "cp", "/tmp"]);
        assert_eq!(
            value_of(&parsed.values, "url"),
            Some("https://example.com/mcp".to_owned())
        );
        assert_eq!(
            parsed.lists.get("env"),
            Some(&vec!["A=1".to_owned(), "B=2".to_owned()])
        );
        assert_eq!(parsed.lists.get("header"), Some(&vec!["X=Y".to_owned()]));
        assert!(error.is_empty());
        Ok(())
    }

    #[test]
    fn unknown_option_is_rejected() {
        let mut error = Vec::new();
        assert!(
            parse_options(
                &["--bogus".to_owned()],
                &[("url", Kind::Value)],
                1,
                &mut error
            )
            .is_none()
        );
        assert!(
            error
                .iter()
                .any(|message| message.contains("Unknown option"))
        );
    }

    #[test]
    fn alias_minus_l_is_local() -> Result<(), String> {
        let mut error = Vec::new();
        let parsed = parse_options(
            &["-l".to_owned()],
            &[("local", Kind::Flag)],
            usize::MAX,
            &mut error,
        )
        .ok_or_else(|| error.join("\n"))?;
        assert!(parsed.values.contains_key("local"));
        assert!(error.is_empty());
        Ok(())
    }
}

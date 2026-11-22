//! Port of `packages/coding-agent/src/core/resolve-config-value.ts`.
//!
//! Configuration values (API keys, headers, MCP settings) may be literals,
//! `$ENV_VAR` / `${ENV_VAR}` templates, or shell commands prefixed with `!`.
//! This module resolves those references without requiring an async runtime.
//!
//! Command results are cached for the lifetime of the process, matching
//! upstream. Environment values are never cached because they can change
//! between calls.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value};

/// A parsed piece of a template string.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplatePart {
    Literal(String),
    Env(String),
}

/// The kind of reference held by a configuration string.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigReference {
    Command(String),
    Template(Vec<TemplatePart>),
}

/// Process-lifetime cache of `!command` results. `None` is cached as a failed
/// resolution so a failing command is executed only once, like upstream.
static COMMAND_CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();

fn command_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    COMMAND_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Clear the command result cache. Exported for tests and callers that change
/// PATH or credentials at runtime.
pub fn clear_config_value_cache() {
    let mut cache = command_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    cache.clear();
}

/// True when `config` is a `!command` value. A bare `!` is also a command,
/// matching upstream's `startsWith("!")`.
pub fn is_command_config_value(config: &str) -> bool {
    config.starts_with('!')
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn append_literal(parts: &mut Vec<TemplatePart>, value: &str) {
    if value.is_empty() {
        return;
    }
    if let Some(TemplatePart::Literal(previous)) = parts.last_mut() {
        previous.push_str(value);
    } else {
        parts.push(TemplatePart::Literal(value.to_string()));
    }
}

/// Parse `$NAME`, `${NAME}`, `$$`, `$!`, and malformed `$` sequences exactly
/// as upstream's `parseConfigValueTemplate` does.
fn parse_template(config: &str) -> Vec<TemplatePart> {
    let mut parts = Vec::new();
    let mut index = 0usize;

    while index < config.len() {
        let Some(relative) = config[index..].find('$') else {
            append_literal(&mut parts, &config[index..]);
            break;
        };
        let dollar = index + relative;
        append_literal(&mut parts, &config[index..dollar]);

        let next = config[dollar + 1..].chars().next();
        match next {
            Some('$') | Some('!') => {
                if let Some(c) = next {
                    append_literal(&mut parts, &c.to_string());
                }
                index = dollar + 2;
            }
            Some('{') => {
                let braces_start = dollar + 2;
                if let Some(relative_end) = config[braces_start..].find('}') {
                    let end = braces_start + relative_end;
                    let name = &config[braces_start..end];
                    if is_env_name(name) {
                        parts.push(TemplatePart::Env(name.to_string()));
                    } else {
                        append_literal(&mut parts, &config[dollar..=end]);
                    }
                    index = end + 1;
                } else {
                    append_literal(&mut parts, "$");
                    index = dollar + 1;
                }
            }
            _ => {
                let tail = &config[dollar + 1..];
                let name_len = tail
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .map(char::len_utf8)
                    .sum::<usize>();
                if name_len > 0 && is_env_name(&tail[..name_len]) {
                    parts.push(TemplatePart::Env(tail[..name_len].to_string()));
                    index = dollar + 1 + name_len;
                } else {
                    append_literal(&mut parts, "$");
                    index = dollar + 1;
                }
            }
        }
    }

    parts
}

fn parse_reference(config: &str) -> ConfigReference {
    if is_command_config_value(config) {
        ConfigReference::Command(config.to_string())
    } else {
        ConfigReference::Template(parse_template(config))
    }
}

/// Environment variables referenced by a value, in first-occurrence order.
pub fn get_config_value_env_var_names(config: &str) -> Vec<String> {
    match parse_reference(config) {
        ConfigReference::Command(_) => Vec::new(),
        ConfigReference::Template(parts) => {
            let mut names = Vec::new();
            for part in parts {
                if let TemplatePart::Env(name) = part
                    && !names.contains(&name)
                {
                    names.push(name);
                }
            }
            names
        }
    }
}

/// The single environment variable a value consists of, if any.
pub fn get_config_value_env_var_name(config: &str) -> Option<String> {
    match parse_reference(config) {
        ConfigReference::Command(_) => None,
        ConfigReference::Template(parts) if parts.len() == 1 => match parts.first() {
            Some(TemplatePart::Env(name)) => Some(name.clone()),
            _ => None,
        },
        ConfigReference::Template(_) => None,
    }
}

fn lookup_env(name: &str, env: Option<&HashMap<String, String>>) -> Option<String> {
    env.and_then(|env| env.get(name))
        .cloned()
        .or_else(|| std::env::var(name).ok())
}

/// Environment variables referenced by `config` that cannot be resolved.
pub fn get_missing_config_value_env_var_names(
    config: &str,
    env: Option<&HashMap<String, String>>,
) -> Vec<String> {
    get_config_value_env_var_names(config)
        .into_iter()
        .filter(|name| lookup_env(name, env).is_none())
        .collect()
}

/// True when every environment variable referenced by `config` resolves.
pub fn is_config_value_configured(config: &str, env: Option<&HashMap<String, String>>) -> bool {
    get_missing_config_value_env_var_names(config, env).is_empty()
}

fn resolve_template(
    parts: &[TemplatePart],
    env: Option<&HashMap<String, String>>,
) -> Option<String> {
    let mut resolved = String::new();
    for part in parts {
        match part {
            TemplatePart::Literal(value) => resolved.push_str(value),
            TemplatePart::Env(name) => resolved.push_str(&lookup_env(name, env)?),
        }
    }
    Some(resolved)
}

/// Shell used to execute `!command` values. Upstream uses the user's configured
/// shell on Windows and `execSync` (a system shell) elsewhere.
fn command_shell(command: &str, provider: &str, option_key: &str) -> Command {
    #[cfg(windows)]
    let mut process = {
        let mut process = Command::new("cmd");
        process.arg("/C");
        process
    };
    #[cfg(not(windows))]
    let mut process = {
        let mut process = Command::new("sh");
        process.arg("-c");
        process
    };
    process.arg(command);
    // Make the config entry discoverable by command-backed values without
    // changing how the command itself is interpreted.
    process.env("PI_CONFIG_PROVIDER", provider);
    process.env("PI_CONFIG_OPTION_KEY", option_key);
    process
}

fn execute_command_uncached(
    command_config: &str,
    provider: &str,
    option_key: &str,
) -> Option<String> {
    let command = &command_config[1..];
    let output = command_shell(command, provider, option_key).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout);
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn execute_command(command_config: &str, provider: &str, option_key: &str) -> Option<String> {
    if let Some(cached) = command_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(command_config)
    {
        return cached.clone();
    }
    let result = execute_command_uncached(command_config, provider, option_key);
    let mut cache = command_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    // Another thread may have resolved the same command while we were waiting.
    cache
        .entry(command_config.to_string())
        .or_insert_with(|| result.clone());
    cache.get(command_config).cloned().flatten()
}

/// Resolve a string config value to `None` when it references an unset
/// environment variable or a command that failed.
fn resolve_string_uncached(
    config: &str,
    provider: &str,
    option_key: &str,
    env: Option<&HashMap<String, String>>,
) -> Option<String> {
    match parse_reference(config) {
        ConfigReference::Command(command) => {
            execute_command_uncached(&command, provider, option_key)
        }
        ConfigReference::Template(parts) => resolve_template(&parts, env),
    }
}

/// Resolve a string config value, using the process-lifetime command cache.
pub fn resolve_config_value_str(config: &str, provider: &str, option_key: &str) -> Option<String> {
    resolve_config_value_str_with_env(config, provider, option_key, None)
}

/// Variant of [`resolve_config_value_str`] with an environment overlay whose
/// values take precedence over the process environment.
pub fn resolve_config_value_str_with_env(
    config: &str,
    provider: &str,
    option_key: &str,
    env: Option<&HashMap<String, String>>,
) -> Option<String> {
    match parse_reference(config) {
        ConfigReference::Command(command) => execute_command(&command, provider, option_key),
        ConfigReference::Template(parts) => resolve_template(&parts, env),
    }
}

fn resolve_value_uncached(
    value: &Value,
    provider: &str,
    option_key: &str,
    env: Option<&HashMap<String, String>>,
) -> Value {
    match value {
        Value::String(config) => {
            let resolved = if env.is_none() {
                resolve_config_value_str(config, provider, option_key)
            } else {
                resolve_string_uncached(config, provider, option_key, env)
            };
            resolved.map_or(Value::Null, Value::String)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| resolve_value_uncached(item, provider, option_key, env))
                .collect(),
        ),
        Value::Object(map) => {
            // A provider/option-keyed object selects one entry; otherwise each
            // value is resolved in place without changing key insertion order.
            let selected = map
                .get(provider)
                .or_else(|| map.get(option_key))
                .or_else(|| map.get("default"))
                .or_else(|| map.get("*"));
            match selected {
                Some(entry) => resolve_value_uncached(entry, provider, option_key, env),
                None => {
                    let resolved = Map::from_iter(map.iter().map(|(key, item)| {
                        (
                            key.clone(),
                            resolve_value_uncached(item, provider, option_key, env),
                        )
                    }));
                    Value::Object(resolved)
                }
            }
        }
        other => other.clone(),
    }
}

/// Resolve a configuration value for `provider` and `option_key`.
///
/// Strings are expanded as `$VAR` / `${VAR}` templates or executed as shell
/// commands when they start with `!`. Unresolvable references become
/// `Value::Null`, matching upstream's `undefined` result. Objects may hold
/// provider- or option-keyed alternatives; other objects and arrays are
/// resolved recursively. `provider` and `option_key` are also exported to
/// command processes as `PI_CONFIG_PROVIDER` and `PI_CONFIG_OPTION_KEY`.
pub fn resolve_config_value(
    value: &Value,
    provider: &str,
    option_key: &str,
) -> Result<Value, String> {
    resolve_config_value_with_env(value, provider, option_key, None)
}

/// Variant of [`resolve_config_value`] with a credential-scoped environment
/// overlay, which takes precedence over process environment variables.
pub fn resolve_config_value_with_env(
    value: &Value,
    provider: &str,
    option_key: &str,
    env: Option<&HashMap<String, String>>,
) -> Result<Value, String> {
    Ok(resolve_value_uncached(value, provider, option_key, env))
}

/// Resolve a string value, returning an error when it does not resolve. The
/// error names the provider and option for diagnostics.
pub fn resolve_config_value_or_error(
    config: &str,
    provider: &str,
    option_key: &str,
) -> Result<String, String> {
    resolve_config_value_str(config, provider, option_key).ok_or_else(|| {
        match parse_reference(config) {
            ConfigReference::Command(_) => format!(
                "Failed to resolve {} for provider \"{}\" from shell command",
                option_key, provider
            ),
            ConfigReference::Template(_) => {
                let missing = get_missing_config_value_env_var_names(config, None);
                if missing.is_empty() {
                    format!(
                        "Failed to resolve {} for provider \"{}\"",
                        option_key, provider
                    )
                } else {
                    format!(
                        "Failed to resolve {} for provider \"{}\" from environment variables: {}",
                        option_key,
                        provider,
                        missing.join(", ")
                    )
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn resolve_string(value: &str) -> Value {
        resolve_config_value(&Value::String(value.to_string()), "openai", "apiKey")
            .unwrap_or(Value::Null)
    }

    #[test]
    fn is_command_config_value_detects_bang_prefix() {
        assert!(is_command_config_value("!echo key"));
        assert!(is_command_config_value("!"));
        assert!(!is_command_config_value("$KEY"));
        assert!(!is_command_config_value("key"));
    }

    #[test]
    fn parses_env_var_names_in_order() {
        assert_eq!(get_config_value_env_var_names("$A-${B}-$A"), vec!["A", "B"]);
        assert_eq!(
            get_config_value_env_var_names("!echo $A"),
            Vec::<String>::new()
        );
        assert_eq!(get_config_value_env_var_name("$A"), Some("A".to_string()));
        assert_eq!(get_config_value_env_var_name("$A-$B"), None);
    }

    #[test]
    fn resolves_literals_and_escapes() {
        assert_eq!(
            resolve_string("literal-key"),
            Value::String("literal-key".into())
        );
        assert_eq!(resolve_string("$$KEY"), Value::String("$KEY".into()));
        assert_eq!(
            resolve_string("$!literal"),
            Value::String("!literal".into())
        );
        assert_eq!(
            resolve_string("${INVALID-NAME}"),
            Value::String("${INVALID-NAME}".into())
        );
    }

    #[test]
    fn resolves_braced_and_unbraced_env_vars() -> Result<(), String> {
        let overlay = env(&[("LEFT", "left"), ("RIGHT", "right")]);
        let value = Value::String("$LEFT-${RIGHT}".to_string());
        let resolved = resolve_config_value_with_env(&value, "openai", "apiKey", Some(&overlay))?;
        assert_eq!(resolved, Value::String("left-right".into()));
        Ok(())
    }

    #[test]
    fn scoped_env_overrides_process_env() {
        let overlay = env(&[("PI_TEST_SCOPED_KEY", "scoped")]);
        let value = Value::String("$PI_TEST_SCOPED_KEY".to_string());
        let resolved = resolve_config_value_with_env(&value, "openai", "apiKey", Some(&overlay))
            .unwrap_or(Value::Null);
        assert_eq!(resolved, Value::String("scoped".into()));
    }

    #[test]
    fn missing_env_var_resolves_to_null() {
        assert_eq!(resolve_string("$PI_TEST_MISSING_KEY_90210"), Value::Null);
        assert_eq!(
            get_missing_config_value_env_var_names("$PI_TEST_MISSING_KEY_90210", None),
            vec!["PI_TEST_MISSING_KEY_90210".to_string()]
        );
        assert!(!is_config_value_configured(
            "$PI_TEST_MISSING_KEY_90210",
            None
        ));
    }

    #[test]
    fn executes_command_and_trims_output() {
        assert_eq!(
            resolve_string("!printf '  command-key  '"),
            Value::String("command-key".into())
        );
    }

    #[test]
    fn failed_or_empty_command_resolves_to_null() {
        assert_eq!(resolve_string("!exit 1"), Value::Null);
        assert_eq!(resolve_string("!printf ''"), Value::Null);
    }

    #[test]
    fn caches_command_results_until_cleared() -> Result<(), String> {
        let root = tempfile::tempdir().map_err(|error| error.to_string())?;
        let counter = root.path().join("counter");
        std::fs::write(&counter, "0").map_err(|error| error.to_string())?;
        let script = format!(
            "!sh -c 'n=$(cat \"{}\"); echo $((n + 1)) > \"{}\"; echo value'",
            counter.display(),
            counter.display()
        );
        clear_config_value_cache();
        assert_eq!(resolve_string(&script), Value::String("value".into()));
        assert_eq!(resolve_string(&script), Value::String("value".into()));
        assert_eq!(
            std::fs::read_to_string(&counter)
                .map_err(|error| error.to_string())?
                .trim(),
            "1"
        );
        clear_config_value_cache();
        assert_eq!(resolve_string(&script), Value::String("value".into()));
        assert_eq!(
            std::fs::read_to_string(&counter)
                .map_err(|error| error.to_string())?
                .trim(),
            "2"
        );
        clear_config_value_cache();
        Ok(())
    }

    #[test]
    fn resolves_provider_and_option_keyed_objects() -> Result<(), String> {
        let value: Value = serde_json::from_str(
            r#"{"openai":"$PI_TEST_PROVIDER_KEY","apiKey":"other","default":"fallback"}"#,
        )
        .map_err(|error| error.to_string())?;
        let overlay = env(&[("PI_TEST_PROVIDER_KEY", "provider-value")]);
        assert_eq!(
            resolve_config_value_with_env(&value, "openai", "apiKey", Some(&overlay))?,
            Value::String("provider-value".into())
        );
        assert_eq!(
            resolve_config_value_with_env(&value, "anthropic", "apiKey", Some(&overlay))?,
            Value::String("other".into())
        );
        Ok(())
    }

    #[test]
    fn resolves_arrays_and_plain_objects_recursively() -> Result<(), String> {
        let value: Value = serde_json::from_str(r#"["$PI_TEST_A",{"x":"$PI_TEST_B"}]"#)
            .map_err(|error| error.to_string())?;
        let overlay = env(&[("PI_TEST_A", "a"), ("PI_TEST_B", "b")]);
        assert_eq!(
            resolve_config_value_with_env(&value, "openai", "headers", Some(&overlay))?,
            serde_json::json!(["a", { "x": "b" }])
        );
        Ok(())
    }

    #[test]
    fn or_error_names_provider_and_missing_vars() {
        let error = resolve_config_value_or_error(
            "$PI_TEST_ABSENT_A-$PI_TEST_ABSENT_B",
            "openai",
            "apiKey",
        )
        .expect_err("unresolved variables must fail");
        assert!(error.contains("openai"));
        assert!(error.contains("PI_TEST_ABSENT_A"));
        assert!(error.contains("PI_TEST_ABSENT_B"));
    }

    #[test]
    fn command_receives_provider_and_option_key_in_environment() {
        let resolved = resolve_config_value_str(
            "!printf '%s/%s' \"$PI_CONFIG_PROVIDER\" \"$PI_CONFIG_OPTION_KEY\"",
            "openai",
            "apiKey",
        );
        assert_eq!(resolved.as_deref(), Some("openai/apiKey"));
    }
}

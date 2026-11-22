//! `packages/mcp/test/config.test.ts`.

use std::sync::{Arc, Mutex};

use pi_rs_mcp::{
    McpServerConfigOptions, McpServerConfigResult, is_mcp_server_override,
    validate_mcp_server_config,
};
use serde_json::json;

/// The error of an invalid entry, or the empty string.
fn error_of(result: &McpServerConfigResult) -> String {
    match result {
        McpServerConfigResult::Invalid(error) => error.clone(),
        _ => String::new(),
    }
}

fn validate(name: &str, raw: serde_json::Value) -> McpServerConfigResult {
    validate_mcp_server_config(name, &raw, &McpServerConfigOptions::default())
}

#[test]
fn accepts_stdio_and_http_entries_and_keeps_them_as_given() {
    let stdio = json!({
        "command": "npx",
        "args": ["-y", "server"],
        "env": { "A": "1" },
        "cwd": "sub",
        "enabled": false,
    });
    assert_eq!(
        validate("files", stdio.clone()),
        McpServerConfigResult::Stdio(stdio.clone())
    );
    assert!(matches!(
        validate("files", json!({ "type": "stdio", "command": "x" })),
        McpServerConfigResult::Stdio(_)
    ));

    let http = json!({
        "url": "https://example.com/mcp",
        "headers": { "Authorization": "Bearer token" },
        "oauth": { "clientId": "id", "callbackPort": 8080, "scope": "read" },
        "timeout": 30,
        "description": "Docs",
    });
    assert_eq!(
        validate("docs", http.clone()),
        McpServerConfigResult::Http(http)
    );
    for kind in ["http", "streamable-http"] {
        assert!(matches!(
            validate(
                "docs",
                json!({ "type": kind, "url": "http://localhost:3000/mcp" })
            ),
            McpServerConfigResult::Http(_)
        ));
    }
}

#[test]
fn picks_the_transport_from_type_url_and_command() {
    // With both, url wins unless the type says stdio.
    assert!(matches!(
        validate("s", json!({ "url": "https://a.example", "command": "x" })),
        McpServerConfigResult::Http(_)
    ));
    assert!(matches!(
        validate(
            "s",
            json!({ "type": "stdio", "url": "https://a.example", "command": "x" })
        ),
        McpServerConfigResult::Stdio(_)
    ));
    assert_eq!(
        error_of(&validate("s", json!({ "type": "http", "command": "x" }))),
        "server \"s\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)"
    );
    assert_eq!(
        error_of(&validate(
            "s",
            json!({ "type": "sse", "url": "https://a.example/sse" })
        )),
        "server \"s\": legacy SSE transport is not supported; use the streamable HTTP URL"
    );
}

#[test]
fn reports_invalid_names_and_fields() {
    let cases = [
        ("bad name", json!({ "command": "x" })),
        ("s", json!("npx")),
        ("s", json!([])),
        ("s", json!({ "command": "x", "enabled": "yes" })),
        ("s", json!({ "command": "x", "description": 1 })),
        ("s", json!({ "command": "x", "timeout": 0 })),
        ("s", json!({ "command": "x", "args": "a b" })),
        ("s", json!({ "command": "x", "env": { "A": 1 } })),
        ("s", json!({ "command": "x", "cwd": 1 })),
        ("s", json!({ "url": "file:///x" })),
        ("s", json!({ "url": "not a url" })),
        (
            "s",
            json!({ "url": "https://a.example", "headers": { "A": 1 } }),
        ),
        ("s", json!({})),
    ];
    let expected = [
        "invalid server name \"bad name\" (use letters, digits, \"_\" and \"-\")",
        "server \"s\" must be an object",
        "server \"s\" must be an object",
        "server \"s\": enabled must be a boolean",
        "server \"s\": description must be a string",
        "server \"s\": timeout must be a positive number of seconds",
        "server \"s\": args must be an array of strings",
        "server \"s\": env must map names to strings",
        "server \"s\": cwd must be a string",
        "server \"s\": url must be an http or https URL",
        "server \"s\": url must be an http or https URL",
        "server \"s\": headers must map names to strings",
        "server \"s\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)",
    ];
    for ((name, raw), expected) in cases.iter().zip(expected.iter()) {
        assert_eq!(
            error_of(&validate(name, raw.clone())),
            *expected,
            "case {name}"
        );
    }
}

#[test]
fn checks_oauth_settings() {
    let oauth = |settings: serde_json::Value| {
        error_of(&validate(
            "s",
            json!({ "url": "https://a.example", "oauth": settings }),
        ))
    };
    assert_eq!(oauth(json!("x")), "server \"s\": oauth must be an object");
    assert_eq!(
        oauth(json!({ "clientId": 1 })),
        "server \"s\": oauth.clientId must be a string"
    );
    assert_eq!(
        oauth(json!({ "clientSecret": 1 })),
        "server \"s\": oauth.clientSecret must be a string"
    );
    assert_eq!(
        oauth(json!({ "callbackPort": 70000 })),
        "server \"s\": oauth.callbackPort must be a port number"
    );
    assert!(
        oauth(json!({ "callbackUrl": "https://localhost/callback" }))
            .contains("oauth.callbackUrl must be an http URI")
    );
    assert!(
        oauth(json!({ "callbackUrl": "http://example.com/callback" }))
            .contains("oauth.callbackUrl must be an http URI")
    );
    assert!(
        oauth(json!({ "callbackUrl": "http://localhost/callback?x=1" }))
            .contains("oauth.callbackUrl must be an http URI")
    );
    assert_eq!(
        oauth(json!({ "callbackUrl": "http://[::1]:8080/cb", "callbackPort": 8080 })),
        ""
    );
    assert_eq!(
        oauth(json!({ "callbackUrl": "http://localhost:8080/cb", "callbackPort": 9090 })),
        "server \"s\": oauth.callbackUrl and oauth.callbackPort name different ports"
    );
    assert_eq!(
        oauth(json!({ "scope": 1 })),
        "server \"s\": oauth.scope must be a string"
    );
    assert_eq!(
        oauth(json!({ "clientName": " " })),
        "server \"s\": oauth.clientName must be a non-empty string"
    );
    assert_eq!(
        oauth(json!({ "clientRegistration": "x" })),
        "server \"s\": oauth.clientRegistration must be \"auto\", \"dcr\", or \"cimd\""
    );
    assert_eq!(
        oauth(json!({ "clientRegistration": "dcr", "clientName": "app" })),
        ""
    );
    assert!(
        oauth(json!({ "authServerMetadataUrl": "http://auth.example/.well-known/x" }))
            .contains("oauth.authServerMetadataUrl must be an https URL")
    );
    assert_eq!(
        oauth(json!({ "authServerMetadataUrl": "http://127.0.0.1/.well-known/x" })),
        ""
    );
}

#[test]
fn allows_cimd_only_with_the_applications_check_of_the_callback() {
    let entry = |oauth: serde_json::Value| json!({ "url": "https://a.example", "oauth": oauth });
    assert_eq!(
        error_of(&validate(
            "s",
            entry(json!({ "clientRegistration": "cimd" }))
        )),
        "server \"s\": oauth.clientRegistration \"cimd\" is not supported: there are no Client ID Metadata Documents"
    );
    let checked: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let options = McpServerConfigOptions {
        check_metadata_document_callback: Some(Arc::new({
            let checked = checked.clone();
            move |callback_url: Option<&str>| {
                checked
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(callback_url.map(str::to_string));
                match callback_url {
                    Some(url) if url.ends_with("/other") => {
                        Some("the documents do not list it".to_string())
                    }
                    _ => None,
                }
            }
        })),
    };
    assert!(matches!(
        validate_mcp_server_config(
            "s",
            &entry(json!({ "clientRegistration": "cimd" })),
            &options
        ),
        McpServerConfigResult::Http(_)
    ));
    assert_eq!(
        error_of(&validate_mcp_server_config(
            "s",
            &entry(
                json!({ "clientRegistration": "cimd", "callbackUrl": "http://localhost/other" })
            ),
            &options,
        )),
        "server \"s\": the documents do not list it"
    );
    assert_eq!(
        checked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
        vec![None, Some("http://localhost/other".to_string())]
    );
    assert_eq!(
        error_of(&validate_mcp_server_config(
            "s",
            &entry(json!({ "clientRegistration": "cimd", "clientId": "id" })),
            &options,
        )),
        "server \"s\": oauth.clientRegistration \"cimd\" cannot be combined with oauth.clientId or oauth.clientName"
    );
}

/// An application's own fields, like p2's `shared` and `runOn`, checked on the result.
fn validate_app_config(name: &str, raw: serde_json::Value) -> String {
    let result = validate(name, raw);
    let config = match &result {
        McpServerConfigResult::Invalid(_) => return error_of(&result),
        McpServerConfigResult::Stdio(config) => config,
        McpServerConfigResult::Http(config) => config,
    };
    let shared = config.get("shared");
    if shared.is_some_and(|shared| !shared.is_boolean()) {
        return format!("server \"{name}\": shared must be a boolean");
    }
    if matches!(result, McpServerConfigResult::Http(_)) && config.get("runOn").is_some() {
        return format!("server \"{name}\": runOn is for stdio");
    }
    let kind = match result {
        McpServerConfigResult::Stdio(_) => "stdio",
        McpServerConfigResult::Http(_) => "http",
        McpServerConfigResult::Invalid(_) => "invalid",
    };
    let shared_text = shared
        .map(|shared| shared.to_string())
        .unwrap_or_else(|| "undefined".to_string());
    format!("{kind} shared={shared_text}")
}

#[test]
fn keeps_unknown_fields_for_the_caller_after_the_shared_fields_are_valid() {
    assert_eq!(
        validate_app_config(
            "s",
            json!({ "command": "x", "shared": true, "runOn": "host" })
        ),
        "stdio shared=true"
    );
    assert_eq!(
        validate_app_config("s", json!({ "url": "https://a.example", "runOn": "host" })),
        "server \"s\": runOn is for stdio"
    );
    assert_eq!(
        validate_app_config("s", json!({ "command": "x", "shared": 1 })),
        "server \"s\": shared must be a boolean"
    );
    // The shared fields are reported first.
    assert_eq!(
        validate_app_config("s", json!({ "command": "x", "shared": 1, "enabled": "no" })),
        "server \"s\": enabled must be a boolean"
    );
}

#[test]
fn override_is_true_for_entries_without_command_url_or_type() {
    assert!(is_mcp_server_override(&json!({ "enabled": false })));
    assert!(is_mcp_server_override(&json!({})));
    assert!(!is_mcp_server_override(&json!({ "command": "x" })));
    assert!(!is_mcp_server_override(
        &json!({ "url": "https://a.example" })
    ));
    assert!(!is_mcp_server_override(
        &json!({ "type": "stdio", "enabled": false })
    ));
}

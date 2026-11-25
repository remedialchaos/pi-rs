//! Offline tests for the standalone `pi mcp` subcommand against the compiled `pi` binary.

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn pi(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
    command
        .current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("PI_CODING_AGENT_DIR", root.join("agent"))
        .env("PI_OFFLINE", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null());
    command
}

fn args(
    root: &Path,
    cmd_args: &[&str],
) -> Result<std::process::Output, Box<dyn std::error::Error>> {
    Ok(pi(root).args(cmd_args).output()?)
}

#[test]
fn mcp_help_and_no_command_print_usage() -> TestResult {
    let root = tempfile::tempdir()?;
    for cmd_args in [&["mcp"][..], &["mcp", "--help"][..], &["mcp", "-h"][..]] {
        let output = args(root.path(), cmd_args)?;
        assert!(output.status.success(), "{cmd_args:?}");
        assert!(output.stderr.is_empty(), "{cmd_args:?}");
        let help = String::from_utf8(output.stdout)?;
        assert!(help.contains("Usage:\n  pi mcp add <server>"), "{help}");
        assert!(help.contains("pi mcp list [--json]"), "{help}");
        assert!(help.contains("pi mcp login <server>"), "{help}");
        assert!(help.contains("pi mcp logout <server>"), "{help}");
    }
    Ok(())
}

#[test]
fn mcp_unknown_command_and_bad_arguments_fail() -> TestResult {
    let root = tempfile::tempdir()?;
    let output = args(root.path(), &["mcp", "frobnicate"])?;
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    assert!(String::from_utf8(output.stderr)?.contains("Unknown mcp command \"frobnicate\"."));

    let output = args(root.path(), &["mcp", "add"])?;
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("Usage: pi mcp add"), "{stderr}");

    let output = args(root.path(), &["mcp", "add", "x"])?;
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("Usage: pi mcp add"), "{stderr}");

    let output = args(
        root.path(),
        &[
            "mcp",
            "add",
            "x",
            "--url",
            "https://example.com",
            "--env",
            "A=1",
        ],
    )?;
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("--env only applies to stdio servers"),
        "{stderr}"
    );

    let output = args(root.path(), &["mcp", "remove"])?;
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    Ok(())
}

#[test]
fn mcp_add_remove_and_list() -> TestResult {
    let root = tempfile::tempdir()?;
    let global = root.path().join("agent/mcp.json");

    // Empty list.
    let output = args(root.path(), &["mcp", "list"])?;
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("No MCP servers configured."), "{stdout}");

    // Add a stdio server.
    let output = args(root.path(), &["mcp", "add", "fs", "--", "/bin/true"])?;
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("Added global MCP server \"fs\""),
        "{stdout}"
    );
    assert!(
        stdout.contains(global.to_string_lossy().as_ref()),
        "{stdout}"
    );
    assert!(global.is_file());

    // Replacing reports replaced.
    let output = args(root.path(), &["mcp", "add", "fs", "--", "/bin/false"])?;
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("Replaced global MCP server \"fs\""),
        "{stdout}"
    );

    // List connects to the server: /bin/false exits immediately, so the connection fails.
    let output = args(root.path(), &["mcp", "list"])?;
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("fs: failed"), "{stdout}");
    assert!(stdout.contains("  /bin/false"), "{stdout}");

    // JSON list.
    let output = args(root.path(), &["mcp", "list", "--json"])?;
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    let report: Value = serde_json::from_str(&String::from_utf8(output.stdout)?)?;
    assert_eq!(report["servers"][0]["name"], "fs");
    assert_eq!(report["servers"][0]["state"], "failed");

    // Remove.
    let output = args(root.path(), &["mcp", "remove", "fs"])?;
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("Removed global MCP server \"fs\""),
        "{stdout}"
    );

    // Removing a missing server fails.
    let output = args(root.path(), &["mcp", "remove", "fs"])?;
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("No global MCP server named \"fs\""),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn mcp_add_http_server() -> TestResult {
    let root = tempfile::tempdir()?;
    let output = args(
        root.path(),
        &["mcp", "add", "docs", "--url", "https://example.com/mcp"],
    )?;
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("Added global MCP server \"docs\""),
        "{stdout}"
    );
    assert!(stdout.contains("pi mcp login docs"), "{stdout}");

    let global = root.path().join("agent/mcp.json");
    let config: Value = serde_json::from_str(&std::fs::read_to_string(&global)?)?;
    assert_eq!(
        config["mcpServers"]["docs"]["url"],
        "https://example.com/mcp"
    );

    Ok(())
}

#[test]
fn mcp_add_with_options() -> TestResult {
    let root = tempfile::tempdir()?;
    let output = args(
        root.path(),
        &[
            "mcp",
            "add",
            "proxy",
            "--url",
            "http://127.0.0.1:9876/mcp",
            "--header",
            "X-Token=abc",
            "--exposure",
            "direct",
            "--description",
            "A demo server",
        ],
    )?;
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let global = root.path().join("agent/mcp.json");
    let config: Value = serde_json::from_str(&std::fs::read_to_string(&global)?)?;
    let server = &config["mcpServers"]["proxy"];
    assert_eq!(server["url"], "http://127.0.0.1:9876/mcp");
    assert_eq!(server["headers"]["X-Token"], "abc");
    assert_eq!(server["exposure"], "direct");
    assert_eq!(server["description"], "A demo server");
    Ok(())
}

#[test]
fn mcp_invalid_exposure_and_bad_url_fail() -> TestResult {
    let root = tempfile::tempdir()?;
    let output = args(
        root.path(),
        &["mcp", "add", "x", "--exposure", "weird", "--", "/bin/true"],
    )?;
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("Invalid exposure"), "{stderr}");

    let output = args(root.path(), &["mcp", "add", "x", "--url", "not a url"])?;
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("server \"x\": url must be an http or https URL"),
        "{stderr}"
    );

    // Both url and command is an error.
    let output = args(
        root.path(),
        &[
            "mcp",
            "add",
            "x",
            "--url",
            "http://127.0.0.1/",
            "--",
            "/bin/true",
        ],
    )?;
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    Ok(())
}

#[test]
fn mcp_local_scoping() -> TestResult {
    let root = tempfile::tempdir()?;
    let project = root.path().join(".pi/mcp.json");

    // Project-trusted is false here, so --local still writes the project file.
    let output = args(
        root.path(),
        &["mcp", "add", "local", "--local", "--", "/bin/true"],
    )?;
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    assert!(project.is_file());

    // Not present in the global file.
    let output = args(root.path(), &["mcp", "remove", "local"])?;
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("No global MCP server named \"local\""),
        "{stderr}"
    );
    assert!(
        stderr.contains(project.to_string_lossy().as_ref()),
        "{stderr}"
    );
    assert!(stderr.contains("use --local"), "{stderr}");

    // Removing with --local works.
    let output = args(root.path(), &["mcp", "remove", "local", "--local"])?;
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("Removed project MCP server \"local\""),
        "{stdout}"
    );
    Ok(())
}

#[test]
fn mcp_login_and_logout_report_errors_offline() -> TestResult {
    let root = tempfile::tempdir()?;
    let output = args(root.path(), &["mcp", "login", "nope"])?;
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("No MCP server named \"nope\""), "{stderr}");

    let output = args(root.path(), &["mcp", "logout", "nope"])?;
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("No MCP server named \"nope\""), "{stderr}");

    let directory = root.path().join("agent");
    std::fs::create_dir_all(&directory)?;
    let url = "https://example.com/mcp";
    let configured = args(root.path(), &["mcp", "add", "docs", "--url", url])?;
    assert!(configured.status.success());
    let tokens = directory.join("mcp-auth.json");
    std::fs::write(
        &tokens,
        serde_json::to_vec(
            &serde_json::json!({"mcp__docs|https://example.com/mcp":{"tokens":{"access_token":"fixture"}},"other":{"tokens":{}}}),
        )?,
    )?;
    let signed_out = args(root.path(), &["mcp", "logout", "docs"])?;
    assert!(signed_out.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(tokens)?)?,
        serde_json::json!({"other":{"tokens":{}}})
    );

    // Add a stdio server, then logout must refuse.
    let _ = args(root.path(), &["mcp", "add", "docs", "--", "example-server"])?;
    let output = args(root.path(), &["mcp", "logout", "docs"])?;
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("does not use OAuth"), "{stderr}");
    Ok(())
}

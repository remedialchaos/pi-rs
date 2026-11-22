use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn pi(agent_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
    command.env_clear().env("PI_CODING_AGENT_DIR", agent_dir);
    command
}

fn assert_output(output: &Output, code: i32, stdout: &str, stderr: &str) {
    assert_eq!(output.status.code(), Some(code));
    assert_eq!(String::from_utf8_lossy(&output.stdout), stdout);
    assert_eq!(String::from_utf8_lossy(&output.stderr), stderr);
}

#[test]
fn auth_help_and_errors_use_the_auth_dispatch() -> TestResult {
    let dir = tempfile::tempdir()?;
    let agent_dir = dir.path().join("agent");
    let bare = pi(&agent_dir).arg("auth").output()?;
    assert_eq!(bare.status.code(), Some(0));
    let help = String::from_utf8(bare.stdout)?;
    assert!(help.starts_with("Usage:\n  pi auth login <provider> [--api-key] [--no-browser]\n"));
    assert!(help.contains("--min-expiry <duration> Minimum remaining token lifetime"));
    let subcommand_help = pi(&agent_dir)
        .args(["auth", "unknown", "--", "-h"])
        .output()?;
    assert_output(&subcommand_help, 0, &help, "");
    let unknown = pi(&agent_dir)
        .args(["auth", "check", "--provider", "openai", "--credentails"])
        .output()?;
    assert_output(
        &unknown,
        1,
        "",
        concat!(
            "Unknown option --credentails for \"auth check\".\n",
            "Use \"pi --help\" or \"pi auth check --provider <provider> [--json] [--credentials] [--no-refresh]\".\n",
        ),
    );
    let diagnostics = pi(&agent_dir)
        .args(["auth", "check", "--provider", "openai", "-z"])
        .output()?;
    assert_output(&diagnostics, 2, "", "Error: Unknown option: -z\n");
    let min_expiry = pi(&agent_dir)
        .args([
            "auth",
            "print-bearer-token",
            "--provider",
            "openai",
            "--min-expiry",
            "0.5h",
        ])
        .output()?;
    assert_output(
        &min_expiry,
        1,
        "",
        "Error: --min-expiry must use a duration such as 30m or 1h\n",
    );
    assert!(!agent_dir.exists());
    Ok(())
}

#[test]
fn auth_check_no_refresh_does_not_create_or_touch_storage() -> TestResult {
    let dir = tempfile::tempdir()?;
    let agent_dir = dir.path().join("agent");
    let auth_path = agent_dir.join("auth.json");
    let args = [
        "auth",
        "check",
        "--no-refresh",
        "--json",
        "--credentials",
        "--provider",
    ];
    let missing = pi(&agent_dir).args(args).arg("openai").output()?;
    assert_output(
        &missing,
        1,
        "{\"status\":\"not_ready\",\"provider\":\"openai\",\"reason\":\"credentials_not_configured\"}\n",
        "",
    );
    assert!(!agent_dir.exists());

    std::fs::create_dir(&agent_dir)?;
    let stored = "\u{feff}{\"openai-codex\":{\"type\":\"oauth\",\"access\":\"old-token\",\"refresh\":\"unused-refresh-token\",\"expires\":0}}\n";
    std::fs::write(&auth_path, stored)?;
    // A held lock does not delay the read-only check or leave a lock side effect.
    let lock_path = agent_dir.join("auth.json.lock");
    std::fs::create_dir(&lock_path)?;
    let refreshed = pi(&agent_dir).args(args).arg("openai-codex").output()?;
    assert_output(
        &refreshed,
        0,
        "{\"status\":\"ready\",\"provider\":\"openai-codex\",\"authType\":\"oauth\",\"credentials\":\"old-token\"}\n",
        "",
    );
    assert_eq!(std::fs::read_to_string(auth_path)?, stored);
    assert_eq!(std::fs::read_dir(lock_path)?.count(), 0);
    assert!(!agent_dir.join("models-store.json").exists());
    Ok(())
}

#[test]
fn auth_check_distinguishes_unknown_provider_invalid_state_and_missing_credential() -> TestResult {
    let dir = tempfile::tempdir()?;
    let args = ["auth", "check", "--no-refresh", "--json", "--provider"];
    let unknown = pi(dir.path()).args(args).arg("nonexistent").output()?;
    assert_output(
        &unknown,
        1,
        "{\"status\":\"not_ready\",\"provider\":\"nonexistent\",\"reason\":\"provider_not_found\"}\n",
        "",
    );
    let path = dir.path().join("auth.json");
    std::fs::write(&path, "this is not JSON")?;
    let invalid = pi(dir.path()).args(args).arg("openai").output()?;
    assert_output(
        &invalid,
        2,
        "{\"status\":\"invalid\",\"provider\":\"openai\",\"reason\":\"invalid_state\"}\n",
        "",
    );
    std::fs::write(
        &path,
        r#"{"openai-codex":{"type":"oauth","access":"","refresh":"unused","expires":0}}"#,
    )?;
    let unavailable = pi(dir.path())
        .args(args)
        .args(["openai-codex", "--credentials"])
        .output()?;
    assert_output(
        &unavailable,
        1,
        "{\"status\":\"not_ready\",\"provider\":\"openai-codex\",\"reason\":\"credential_not_available\"}\n",
        "",
    );
    Ok(())
}

#[test]
fn auth_print_resolves_model_key_and_oauth_authorization_header() -> TestResult {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("auth.json"),
        r#"{
        "openai": {"type":"api_key","key":"${STORED_OPENAI_KEY}","env":{"STORED_OPENAI_KEY":"key-from-overlay"}},
        "anthropic": {"type":"api_key","key":"other-key"},
        "kimi-coding": {"type":"oauth","access":"header-token","refresh":"unused","expires":4102444800000}
    }"#,
    )?;
    let selected = pi(dir.path())
        .args(["auth", "print-api-key", "--model", "gpt-5.5"])
        .output()?;
    assert_output(&selected, 0, "key-from-overlay\n", "");
    let bearer = pi(dir.path())
        .args(["auth", "print-bearer-token", "--provider", "kimi-coding"])
        .output()?;
    assert_output(&bearer, 0, "header-token\n", "");
    let wrong_kind = pi(dir.path())
        .args(["auth", "print-api-key", "--provider", "kimi-coding"])
        .output()?;
    assert_output(
        &wrong_kind,
        1,
        "",
        "Error: Provider \"kimi-coding\" is configured with OAuth, not an API key\n",
    );
    Ok(())
}

#[test]
fn auth_logout_removes_only_the_stored_credentials() -> TestResult {
    let dir = tempfile::tempdir()?;
    let auth = dir.path().join("auth.json");
    let models = dir.path().join("models.json");
    let config = r#"{"providers":{"openai":{"apiKey":"configured-key"}}}"#;
    std::fs::write(&models, config)?;
    std::fs::write(
        &auth,
        r#"{
        "openai": {"type":"api_key","key":"stored-key"},
        "example-extension": {"type":"oauth","access":"token","refresh":"unused","expires":0}
    }"#,
    )?;
    let removed = pi(dir.path())
        .args(["auth", "logout", "OpenAI", "--offline"])
        .output()?;
    assert_output(
        &removed,
        0,
        "Removed stored API key for OpenAI. Environment variables and models.json config are unchanged.\n",
        "",
    );
    let check = pi(dir.path())
        .args([
            "auth",
            "check",
            "--provider",
            "openai",
            "--no-refresh",
            "--credentials",
        ])
        .output()?;
    assert_output(&check, 0, "configured-key\n", "");
    let external = pi(dir.path())
        .args(["auth", "logout", "example-extension"])
        .output()?;
    assert_output(&external, 0, "Logged out of example-extension.\n", "");
    let repeated = pi(dir.path()).args(["auth", "logout", "OpenAI"]).output()?;
    assert_output(
        &repeated,
        0,
        "No stored credentials for OpenAI. Environment variables and models.json config are unchanged.\n",
        "",
    );
    let login = pi(dir.path())
        .args(["auth", "login", "OpenAI", "--api-key"])
        .output()?;
    assert_output(
        &login,
        1,
        "",
        "\"pi auth login <provider> [--api-key] [--no-browser]\" requires an interactive terminal\n",
    );
    assert_eq!(std::fs::read_to_string(models)?, config);
    assert_eq!(std::fs::read_to_string(auth)?, "{}");
    Ok(())
}

#[test]
fn auth_bearer_print_refreshes_and_persists_a_token_below_min_expiry() -> TestResult {
    let dir = tempfile::tempdir()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let done = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&done);
    let server = std::thread::spawn(move || serve_refresh(&listener, &stop));
    let auth = dir.path().join("auth.json");
    let config = json!({"providers": {"radius": {"oauth":"radius","baseUrl":format!("http://{address}/v1")}}});
    std::fs::write(dir.path().join("models.json"), serde_json::to_vec(&config)?)?;
    let credential = json!({"radius":{"type":"oauth","access":"old","refresh":"prior-refresh","expires":chrono::Utc::now().timestamp_millis() + 10 * 60_000}});
    std::fs::write(&auth, serde_json::to_vec(&credential)?)?;
    let printed = pi(dir.path())
        .args([
            "auth",
            "print-bearer-token",
            "--provider",
            "radius",
            "--min-expiry",
            "1h",
        ])
        .output();
    done.store(true, Ordering::Release);
    assert_output(&printed?, 0, "fresh-token\n", "");
    server.join().map_err(|_| "refresh server panicked")??;
    let persisted: Value = serde_json::from_slice(&std::fs::read(auth)?)?;
    assert_eq!(persisted["radius"]["access"], "fresh-token");
    assert_eq!(persisted["radius"]["refresh"], "rolled-refresh");
    assert!(!dir.path().join("auth.json.lock").exists());
    Ok(())
}

fn serve_refresh(listener: &TcpListener, stop: &AtomicBool) -> Result<(), String> {
    let inner = || -> TestResult {
        let cutoff = Instant::now() + Duration::from_secs(15);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if stop.load(Ordering::Acquire) || Instant::now() >= cutoff {
                        return Err("OAuth refresh request was not received".into());
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error.into()),
            }
        };
        socket.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut request = BufReader::new(&mut socket);
        let mut line = String::new();
        request.read_line(&mut line)?;
        assert_eq!(line, "POST /v1/oauth/token HTTP/1.1\r\n");
        let mut length = None;
        loop {
            line.clear();
            if request.read_line(&mut line)? == 0 || line == "\r\n" {
                break;
            }
            if let Some((name, length_str)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = Some(length_str.trim().parse::<usize>()?);
            }
        }
        let mut request_body = vec![0; length.ok_or("missing request body length")?];
        request.read_exact(&mut request_body)?;
        let request_body = String::from_utf8(request_body)?;
        assert!(request_body.contains("grant_type=refresh_token"));
        assert!(request_body.contains("refresh_token=prior-refresh"));
        let response =
            r#"{"access_token":"fresh-token","refresh_token":"rolled-refresh","expires_in":7200}"#;
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        )?;
        Ok(())
    };
    inner().map_err(|error| error.to_string())
}

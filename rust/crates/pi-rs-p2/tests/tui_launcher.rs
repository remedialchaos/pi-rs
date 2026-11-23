use std::{net::TcpListener, process::Command};

#[test]
fn owner_and_share_links_are_rejected_before_contacting_another_origin() -> std::io::Result<()> {
    let target = TcpListener::bind(("127.0.0.1", 0))?;
    target.set_nonblocking(true)?;
    let directory = tempfile::tempdir()?;
    let origin = format!("http://{}", target.local_addr()?);
    for (flag, path, reason) in [
        (
            "--owner-link",
            format!("owner#{}", "a".repeat(64)),
            "owner link for this origin",
        ),
        (
            "--join",
            format!("join/{}", "b".repeat(43)),
            "share link for this origin",
        ),
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_p2"))
            .args([
                "tui",
                &origin,
                flag,
                &format!("http://localhost:{}/{}", target.local_addr()?.port(), path),
            ])
            .env_remove("P2_TUI_OWNER_LINK")
            .env("P2_TUI_CONFIG_DIR", directory.path())
            .env("P2_DATA_DIR", directory.path().join("server"))
            .env("PI_CODING_AGENT_DIR", directory.path().join("agent"))
            .output()?;
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(reason));
        assert!(matches!(target.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
    }
    Ok(())
}

#[test]
fn guest_redemption_keeps_owner_identity_separate() -> Result<(), Box<dyn std::error::Error>> {
    use std::{
        io::{BufRead, BufReader, Read, Write},
        process::Stdio,
    };
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let origin = format!("http://{}", listener.local_addr()?);
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(
            &serde_json::json!({"connections":{&origin:{"cookie":"owner=keep","guest":{"cookie":"p2_guest_7=prior"}}}}),
        )?,
    )?;
    let token = "b".repeat(43);
    let mut child = Command::new(env!("CARGO_BIN_EXE_p2"))
        .args(["tui", "--join", &format!("{origin}/join/{token}")])
        .env_remove("P2_TUI_OWNER_LINK")
        .env("P2_TUI_CONFIG_DIR", dir.path())
        .env("P2_DATA_DIR", dir.path().join("server"))
        .env("PI_CODING_AGENT_DIR", dir.path().join("agent"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("Missing name prompt input")?
        .write_all(b"Guest Reader\n")?;
    listener.set_nonblocking(true)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut socket = loop {
        match listener.accept() {
            Ok((socket, _)) => break socket,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline =>
            {
                if child.try_wait()?.is_some() {
                    return Err("Launcher exited before guest redemption".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => {
                child.kill()?;
                child.wait()?;
                return Err(e.into());
            }
        }
    };
    socket.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut reader = BufReader::new(&socket);
    let mut headers = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err("Truncated join request".into());
        }
        if line == "\r\n" {
            break;
        }
        headers.push_str(&line);
    }
    let headers = headers.to_ascii_lowercase();
    assert!(headers.starts_with("post /join "));
    assert!(headers.contains("\r\ncookie: p2_guest_7=prior\r\n"));
    assert!(!headers.contains("owner=keep"));
    let len = headers
        .lines()
        .find_map(|s| s.strip_prefix("content-length: "))
        .ok_or("Missing join size")?
        .parse()?;
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body)?,
        serde_json::json!({"token":token,"name":"Guest Reader"})
    );
    let body = r#"{"session":"launcher-guest"}"#;
    drop(listener);
    write!(
        socket,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nSet-Cookie: p2_guest_7=updated; HttpOnly\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    drop(socket);
    assert!(!child.wait()?.success()); // The disposable service only serves admission.
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(config_path)?)?;
    assert_eq!(
        saved["connections"][&origin],
        serde_json::json!({"cookie":"owner=keep","guest":{"cookie":"p2_guest_7=updated","session":"launcher-guest"}})
    );
    Ok(())
}

#[tokio::test]
async fn saved_cookie_checks_stop_at_an_origin_redirect() -> Result<(), Box<dyn std::error::Error>>
{
    use axum::{Router, http::StatusCode, response::Redirect, routing::get};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let destination = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let redirect = format!("http://{}/check", destination.local_addr()?);
    let visits = Arc::new(AtomicUsize::new(0));
    let seen = visits.clone();
    let redirected = Router::new().route(
        "/check",
        get(move || async move {
            seen.fetch_add(1, Ordering::Relaxed);
            StatusCode::FORBIDDEN
        }),
    );
    let sink = tokio::spawn(async move { axum::serve(destination, redirected).await });
    let source = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let origin = format!("http://{}", source.local_addr()?);
    let app = Router::new().route(
        "/owner/check",
        get(move || async move { Redirect::temporary(&redirect) }),
    );
    let server = tokio::spawn(async move { axum::serve(source, app).await });
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("config.json"),
        serde_json::to_vec(
            &serde_json::json!({"connections":{&origin:{"cookie":"p2_owner=test"}}}),
        )?,
    )?;
    let result = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_p2"))
            .args(["tui", &origin])
            .env_remove("P2_TUI_OWNER_LINK")
            .env("P2_TUI_CONFIG_DIR", dir.path())
            .env("P2_DATA_DIR", dir.path().join("server"))
            .output()
    })
    .await??;
    server.abort();
    sink.abort();
    assert_eq!(visits.load(Ordering::Relaxed), 0);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Cannot check owner access"));
    Ok(())
}

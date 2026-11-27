#![cfg(unix)]
use axum::{
    Json, Router,
    routing::{get, post},
};
use pi_rs_p2_mac::{
    dev_control, login_environment,
    server_config::{Environment, Mode, ServerConfig, Source, environment},
    server_controller::{Ownership, Phase, ServerController},
    server_scan,
};
use serde_json::{Value, json};
use std::{
    fs,
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::net::UnixListener;
#[path = "support/fixture.rs"]
mod fixture;
mod support;
use support::{Loopback, Test, sandbox};

// The p2 server's observable contract, without its dependencies or providers: the fixture binary
// answers `/health`, writes the start and stop files, and stops on SIGTERM.
fn with_fixture(root: &Path, graceful: bool) -> Test<ServerController> {
    let mut env = environment();
    env.insert("HOME".into(), root.to_string_lossy().into());
    env.insert(
        "FIXTURE_STOP".into(),
        if graceful { "graceful" } else { "hard" }.into(),
    );
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?
        .local_addr()?
        .port();
    use std::os::unix::fs::PermissionsExt;
    let binary = root.join("start-fixture");
    let child = root.join("p2");
    fs::copy(std::env::current_exe()?, &child)?;
    env.insert("FIXTURE_EXE".into(), child.to_string_lossy().into());
    fs::write(
        &binary,
        "#!/bin/sh\nexec \"$FIXTURE_EXE\" --exact fixture::child\n",
    )?;
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))?;
    let config = ServerConfig {
        source: Source::Own,
        binary: Some(binary),
        port,
        data_dir: root.join("private-data"),
        mode: Mode::Start,
        fake: true,
        runs_bundled: false,
    };
    Ok(
        ServerController::new(config, root, env)?.with_stop_grace(if graceful {
            Duration::from_secs(2)
        } else {
            Duration::from_millis(450)
        }),
    )
}

async fn wait_running(server: &mut ServerController) -> Test {
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            server.poll().await?;
            match server.state().phase {
                Phase::Running => return Ok(()),
                Phase::Failed(error) => return Err(pi_rs_p2_mac::Error::Protocol(error)),
                _ => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn real_child_is_ready_only_after_p2_health_and_stops_with_sigterm() -> Test {
    let root = sandbox()?;
    let mut server = with_fixture(root.path(), true)?;
    server.poll().await?;
    assert_eq!(server.state().phase, Phase::Stopped);
    server.start().await?;
    assert_eq!(server.state().phase, Phase::Starting);
    wait_running(&mut server).await?;
    assert_eq!(server.state().ownership, Ownership::Managed);
    let started: Value =
        serde_json::from_reader(fs::File::open(server.config.data_dir.join("started"))?)?;
    assert_eq!(started["fake"], "1");
    assert!(
        fs::read_to_string(&server.log_file)?.contains(
            &server
                .config
                .binary
                .as_ref()
                .ok_or("missing fixture binary")?
                .display()
                .to_string()
        )
    );
    let pid = u32::try_from(started["pid"].as_u64().ok_or("no child PID")?)?;
    let info = server_scan::scan(&root.path().join("default")).await?;
    let scanned = info
        .iter()
        .find(|s| s.port == server.config.port)
        .ok_or_else(|| {
            format!(
                "unsuccessful scan of real p2 child listener on port {} (pid {pid}); found={info:?}",
                server.config.port
            )
        })?;
    assert_eq!(scanned.pid, i32::try_from(pid)?);
    assert_eq!(scanned.data_dir, server.config.data_dir);
    let mut attached = ServerController::new(
        server
            .config
            .attached(server.config.port, server.config.data_dir.clone()),
        root.path(),
        Environment::new(),
    )?;
    attached.poll().await?;
    assert_eq!(attached.state().ownership, Ownership::External);
    attached.stop().await?;
    assert_eq!(attached.state().phase, Phase::Running);
    server.stop().await?;
    assert_eq!(
        fs::read_to_string(server.config.data_dir.join("term"))?,
        "received"
    );
    assert_eq!(server.state().phase, Phase::Stopped);
    assert_eq!(server.state().ownership, Ownership::None);
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, server.config.port)).is_err());
    Ok(())
}

#[tokio::test]
async fn real_child_ignoring_sigterm_is_killed_after_grace() -> Test {
    let root = sandbox()?;
    let mut server = with_fixture(root.path(), false)?;
    server.start().await?;
    wait_running(&mut server).await?;
    let deadline = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(5), server.stop()).await??;
    assert!(deadline.elapsed() >= Duration::from_millis(420));
    assert!(server.config.data_dir.join("term").exists());
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, server.config.port)).is_err());
    Ok(())
}

#[tokio::test]
async fn attached_dev_restart_uses_unix_http_and_leaves_external_process_running() -> Test {
    let dir = sandbox()?;
    let path = dir.path().join("dev.sock");
    let listener = UnixListener::bind(&path)?;
    let (restarted, event) = tokio::sync::oneshot::channel::<Value>();
    let restarted = std::sync::Arc::new(std::sync::Mutex::new(Some(restarted)));
    let reporter = restarted.clone();
    let app = Router::new().route(
        "/",
        post(move |Json(value): Json<Value>| {
            let reporter = reporter.clone();
            async move {
                if let Some(sender) = reporter.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = sender.send(value);
                }
                Json(json!({"ok":true,"message":"replacement ready"}))
            }
        }),
    );
    let unix = tokio::spawn(async move { axum::serve(listener, app).await });
    let upstream = Loopback::serve(
        Router::new().route("/health", get(|| async { Json(json!({"name":"p2"})) })),
    )
    .await?;
    let config = ServerConfig {
        source: Source::Attached,
        binary: None,
        port: upstream.url.port().ok_or("missing port")?,
        data_dir: dir.path().into(),
        mode: Mode::Dev,
        fake: false,
        runs_bundled: false,
    };
    let mut app = ServerController::new(config, dir.path(), Environment::new())?;
    app.poll().await?;
    assert_eq!(app.state().ownership, Ownership::External);
    assert!(app.can_restart());
    app.restart().await?;
    assert_eq!(event.await?, json!({"action":"restart"}));
    app.stop().await?;
    app.poll().await?;
    assert_eq!(app.state().phase, Phase::Running);
    assert!(dev_control::available(dir.path()));
    unix.abort();
    Ok(())
}

#[tokio::test]
async fn login_shell_capture_removes_shell_bookkeeping_and_resolves_node_on_path() -> Test {
    use std::os::unix::fs::PermissionsExt;
    let root = sandbox()?;
    let shell = root.path().join("shell");
    // The fixture emits a login banner and a variable with an embedded '='; this is the capture wire format.
    fs::write(
        &shell,
        "#!/bin/sh\nprintf 'banner\\n__P2MAC_ENV__PATH=/not-found:/usr/bin\\000MCP_VAR=a=b\\000PWD=/shell\\000SHLVL=19\\000__P2MAC_ENV__'\n",
    )?;
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700))?;
    let env = login_environment::capture(&shell, &Environment::new()).await;
    assert_eq!(env.get("MCP_VAR").map(String::as_str), Some("a=b"));
    assert!(!env.contains_key("PWD"));
    assert!(!env.contains_key("SHLVL"));
    assert_eq!(
        login_environment::which("env", env.get("PATH").map(String::as_str)),
        Some(PathBuf::from("/usr/bin/env"))
    );
    Ok(())
}

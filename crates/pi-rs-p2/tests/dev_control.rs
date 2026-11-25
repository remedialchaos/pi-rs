#![cfg(unix)]
use pi_rs_p2::hosts::dev::{DevCommand, DevControl, DevResult, channel};
use std::{collections::BTreeSet, sync::Arc};

#[tokio::test]
async fn private_reload_applies_requested_plugins_and_releases_socket()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let socket = dir.path().join("dev.sock");
    let refuse = |_| async {
        DevResult {
            ok: false,
            message: "refused".into(),
            plugins: None,
        }
    };
    std::fs::write(&socket, b"keep")?;
    assert!(channel::serve(dir.path(), refuse).await.is_err());
    assert_eq!(std::fs::read(&socket)?, b"keep");
    std::fs::remove_file(&socket)?;
    drop(std::os::unix::net::UnixListener::bind(&socket)?);
    let file = dir.path().join("applied");
    let control = Arc::new(DevControl::default());
    let server = channel::serve(dir.path(), move |command| {
        let file = file.clone();
        let control = control.clone();
        async move {
            let DevCommand::Reload { plugins } = command else {
                return DevResult {
                    ok: false,
                    message: "Restart needs the npm run dev supervisor".into(),
                    plugins: None,
                };
            };
            let applied = control
                .reload(
                    plugins,
                    BTreeSet::new(),
                    |names| async move {
                        std::fs::write(file, names.join(",")).map_err(|e| e.to_string())?;
                        Ok((names, ()))
                    },
                    str::to_owned,
                )
                .await;
            let status = control.status(BTreeSet::new(), vec![]).await;
            DevResult {
                ok: applied.is_ok(),
                message: status.message.unwrap_or_default(),
                plugins: Some(status.plugins),
            }
        }
    })
    .await?;
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&socket)?.permissions().mode() & 0o777,
        0o600
    );
    assert!(channel::serve(dir.path(), refuse).await.is_err());
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for (body, status, message) in [
        (b"{".to_vec(), 400, "Invalid dev command JSON"),
        (vec![b' '; 16385], 400, "Dev command too large"),
        (
            br#"{"action":"reload","plugins":["Bad"]}"#.to_vec(),
            400,
            "Invalid plugin names",
        ),
        (
            br#"{"action":"restart"}"#.to_vec(),
            409,
            "Restart needs the npm run dev supervisor",
        ),
    ] {
        let mut wire = tokio::net::UnixStream::connect(&socket).await?;
        let headers = format!(
            "POST / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        wire.write_all(headers.as_bytes()).await?;
        wire.write_all(&body).await?;
        let mut response = String::new();
        wire.read_to_string(&mut response).await?;
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status} ")),
            "{response}"
        );
        assert!(response.contains(message), "{response}");
    }
    assert!(!channel::send(dir.path(), &DevCommand::Restart).await?.ok);
    let result = channel::send(
        dir.path(),
        &DevCommand::Reload {
            plugins: Some(vec!["chat".into(), "chat".into()]),
        },
    )
    .await?;
    assert!(result.ok);
    assert_eq!(result.message, "Reloaded chat");
    assert_eq!(std::fs::read_to_string(dir.path().join("applied"))?, "chat");
    let mut pending = tokio::net::UnixStream::connect(dir.path().join("dev.sock")).await?;
    pending
        .write_all(b"POST / HTTP/1.1\r\nHost: localhost\r\n")
        .await?;
    server.close().await;
    let mut byte = [0];
    match tokio::time::timeout(std::time::Duration::from_secs(2), pending.read(&mut byte)).await? {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => return Err(format!("private connection survived shutdown: {other:?}").into()),
    }
    assert!(!dir.path().join("dev.sock").exists());
    let large = channel::serve(dir.path(), |_| async {
        DevResult {
            ok: true,
            message: "x".repeat(256 * 1024),
            plugins: None,
        }
    })
    .await?;
    assert_eq!(
        channel::send(dir.path(), &DevCommand::Restart)
            .await
            .err()
            .ok_or("oversized result accepted")?
            .to_string(),
        "Dev result too large"
    );
    std::fs::remove_file(&socket)?;
    std::fs::write(&socket, b"replacement")?;
    large.close().await;
    assert_eq!(std::fs::read(&socket)?, b"replacement");
    Ok(())
}

#[tokio::test]
async fn competing_reload_is_refused_and_close_waits_for_the_active_reload()
-> Result<(), Box<dyn std::error::Error>> {
    let control = Arc::new(DevControl::default());
    let (started, ready) = tokio::sync::oneshot::channel();
    let (finish, release) = tokio::sync::oneshot::channel();
    let running = control.clone();
    let active = tokio::spawn(async move {
        running
            .reload(
                None,
                ["chat".into()].into(),
                |plugins| async move {
                    started.send(()).map_err(|_| "start receiver closed")?;
                    release.await.map_err(|_| "release sender closed")?;
                    Ok((plugins.clone(), plugins))
                },
                str::to_owned,
            )
            .await
    });
    ready.await?;
    let attempt = || {
        control.reload(
            None,
            BTreeSet::new(),
            |names| async { Ok((names, ())) },
            str::to_owned,
        )
    };
    assert_eq!(
        attempt().await,
        Err("A dev command is already running".into())
    );
    let closing = control.close();
    tokio::pin!(closing);
    tokio::select! {biased; ()=&mut closing=>return Err("closed before active reload finished".into()), ()=std::future::ready(())=>{}}
    assert_eq!(attempt().await, Err("Dev server is closing".into()));
    finish.send(()).map_err(|_| "active reload exited early")?;
    assert_eq!(active.await??, ["chat"]);
    closing.await;
    Ok(())
}

#[test]
fn worker_baselines_detect_edits_reverts_deletion_and_worker_exit() -> std::io::Result<()> {
    use pi_rs_p2::hosts::dev::{LoadedModules, ModuleReport, WorkerReport};
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("module");
    let reports = dir.path().join("reports");
    let roots = [dir.path().to_owned()];
    let mut loaded = LoadedModules::default();
    let mut old = ModuleReport::default();
    old.record(file.clone(), b"old");
    let worker = WorkerReport::new(&reports, std::process::id())?;
    worker.publish(&old)?;
    loaded.refresh_reports(&reports, &roots, |_| true);
    std::fs::write(&file, b"old")?;
    assert!(loaded.changed().is_empty());
    std::fs::write(&file, b"new")?;
    let mut fresh = ModuleReport::default();
    fresh.record(file.clone(), b"new");
    loaded.receive("server", &fresh.message(), &roots);
    assert_eq!(loaded.changed(), [file.as_path()].into());
    loaded.delete("server");
    std::fs::write(&file, b"old")?;
    assert!(loaded.changed().is_empty());
    std::fs::remove_file(&file)?;
    assert_eq!(loaded.changed(), [file.as_path()].into());
    drop(worker);
    loaded.refresh_reports(&reports, &roots, |_| true);
    assert!(loaded.changed().is_empty());
    Ok(())
}

#[tokio::test]
async fn standalone_process_exposes_private_commands_only_in_dev_mode()
-> Result<(), Box<dyn std::error::Error>> {
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        process::Command,
        time::{Duration, timeout},
    };
    let directory = tempfile::tempdir()?;
    for (flag, env) in [(true, "0"), (false, "1"), (false, "0")] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_p2"));
        if flag {
            command.arg("--dev");
        }
        let mut child = command
            .env("P2_DATA_DIR", directory.path())
            .env("P2_DEV", env)
            .env("P2_FAKE", "1")
            .env("PORT", "0")
            .env_remove("P2_PROVIDER")
            .env_remove("P2_MODEL")
            .env_remove("PI_MOBILE_PROVIDER")
            .env_remove("PI_MOBILE_MODEL")
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().ok_or("missing p2 stdout")?;
        let ready = timeout(
            Duration::from_secs(30),
            BufReader::new(stdout).lines().next_line(),
        )
        .await??;
        assert!(ready.is_some_and(|line| line.starts_with("p2: http://127.0.0.1:")));
        if flag || env == "1" {
            for (command, expected) in [
                (
                    DevCommand::Reload { plugins: None },
                    serde_json::json!({"ok":true,"message":"No dirty plugins","plugins":[]}),
                ),
                (
                    DevCommand::Reload {
                        plugins: Some(vec!["missing-plugin".into()]),
                    },
                    serde_json::json!({"ok":false,"message":"Reload refused: Unknown p2 plugin: missing-plugin"}),
                ),
                (
                    DevCommand::Restart,
                    serde_json::json!({"ok":false,"message":"Restart refused: Restart needs the npm run dev supervisor"}),
                ),
            ] {
                assert_eq!(
                    serde_json::to_value(channel::send(directory.path(), &command).await?)?,
                    expected
                );
            }
        } else {
            assert!(!directory.path().join("dev.sock").exists());
        }
        let signal = Command::new("kill")
            .arg("-TERM")
            .arg(child.id().ok_or("p2 exited early")?.to_string())
            .status()
            .await?;
        assert!(signal.success());
        assert!(
            timeout(Duration::from_secs(15), child.wait())
                .await??
                .success()
        );
        assert!(!directory.path().join("dev.sock").exists());
    }
    Ok(())
}

#[tokio::test]
async fn dev_reload_runs_rebuild_hook_and_reloads_dirty_wasm_plugins()
-> Result<(), Box<dyn std::error::Error>> {
    use pi_rs_chord::{
        context::Context,
        contracts::{
            Contract, Member,
            schema::{int, method},
        },
        json::JsonValue,
    };
    use pi_rs_p2::hosts::{
        dev::DevRebuildHook,
        server::{ServerOptions, start_server},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    const V1: &[u8] = include_bytes!("../../pi-rs-chord/tests/fixtures/b3/v1.wasm");
    const DOUBLE: &[u8] = include_bytes!("../../pi-rs-chord/tests/fixtures/b3/double.wasm");

    let dir = tempfile::tempdir()?;
    let plugins_dir = dir.path().join("plugins");
    std::fs::create_dir_all(&plugins_dir)?;
    std::fs::write(plugins_dir.join("counter.server.wasm"), V1)?;

    // Mode 0: write DOUBLE; Mode 1: fail rebuild; Mode 2: no-op (use on-disk dirty artifact).
    let mode = Arc::new(AtomicUsize::new(0));
    let hook_mode = mode.clone();
    let hook: DevRebuildHook = Arc::new(move |req| {
        let mode = hook_mode.load(Ordering::SeqCst);
        Box::pin(async move {
            match mode {
                0 => {
                    std::fs::write(req.plugins_dir.join("counter.server.wasm"), DOUBLE)
                        .map_err(|e| e.to_string())?;
                    Ok(())
                }
                1 => Err("simulated build failure".into()),
                _ => Ok(()),
            }
        })
    });

    let mut options = ServerOptions::new(dir.path().to_owned());
    options.port = 0;
    options.dev = true;
    options.fake = true;
    options.dev_rebuild = Some(hook);
    let server = start_server(options).await?;

    let ctx = Context::background();
    let contract = Arc::new(Contract::local(
        "guest-counter",
        1,
        vec![],
        indexmap::IndexMap::from([
            ("total".into(), Member::State(int())),
            ("add".into(), method(Some(int()), Some(int()))),
        ]),
    )?);
    let local = server
        .host
        .hold(contract, JsonValue::Object(Default::default()), &ctx)?;
    tokio::time::timeout(std::time::Duration::from_secs(10), local.wait(&ctx)).await??;
    assert_eq!(
        local.call("add", JsonValue::Number(2.), &ctx).await?,
        JsonValue::Number(2.)
    );

    // 1. Rebuild hook writes updated WASM and `server.reload` reports the rebuilt plugin.
    let reloaded = channel::send(dir.path(), &DevCommand::Reload { plugins: None }).await?;
    assert!(reloaded.ok, "{}", reloaded.message);
    assert_eq!(reloaded.plugins, Some(vec!["counter".to_owned()]));
    assert_eq!(reloaded.message, "Reloaded counter");
    assert_eq!(
        local.call("add", JsonValue::Number(3.), &ctx).await?,
        JsonValue::Number(6.)
    );

    // 2. Failing rebuild hook refuses reload and leaves the previous good plugin live.
    mode.store(1, Ordering::SeqCst);
    let refused = channel::send(dir.path(), &DevCommand::Reload { plugins: None }).await?;
    assert!(!refused.ok);
    assert!(
        refused.message.contains("simulated build failure"),
        "{}",
        refused.message
    );
    assert_eq!(
        local.call("add", JsonValue::Number(3.), &ctx).await?,
        JsonValue::Number(12.)
    );

    // 3. Direct on-disk dirty WASM update with no-op hook is detected and reloaded.
    mode.store(2, Ordering::SeqCst);
    std::fs::write(plugins_dir.join("counter.server.wasm"), V1)?;
    let dirty_reloaded = channel::send(dir.path(), &DevCommand::Reload { plugins: None }).await?;
    assert!(dirty_reloaded.ok, "{}", dirty_reloaded.message);
    assert_eq!(dirty_reloaded.plugins, Some(vec!["counter".to_owned()]));
    assert_eq!(
        local.call("add", JsonValue::Number(3.), &ctx).await?,
        JsonValue::Number(3.)
    );

    local.release();
    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn supervised_dev_server_restarts_cleanly_and_supports_rebuild_cmd()
-> Result<(), Box<dyn std::error::Error>> {
    use pi_rs_p2::hosts::dev::supervisor::{DevSupervisor, SupervisorOptions};
    use tokio::time::{Duration, timeout};

    const V1: &[u8] = include_bytes!("../../pi-rs-chord/tests/fixtures/b3/v1.wasm");
    const DOUBLE: &[u8] = include_bytes!("../../pi-rs-chord/tests/fixtures/b3/double.wasm");

    let dir = tempfile::tempdir()?;
    let plugins_dir = dir.path().join("plugins");
    std::fs::create_dir_all(&plugins_dir)?;
    std::fs::write(plugins_dir.join("counter.server.wasm"), V1)?;
    let replacement = dir.path().join("double.wasm");
    std::fs::write(&replacement, DOUBLE)?;

    let rebuild_cmd = format!(
        "cp '{}' \"$P2_PLUGINS_DIR/counter.server.wasm\"",
        replacement.display()
    );

    let supervisor = DevSupervisor::start(SupervisorOptions {
        binary: env!("CARGO_BIN_EXE_p2").into(),
        data_dir: dir.path().to_owned(),
        port: 0,
        fake: true,
        rebuild_cmd: Some(rebuild_cmd),
        extra_env: Vec::new(),
        extra_args: Vec::new(),
    })
    .await?;

    let mut state = supervisor.state();
    let gen1 = state.borrow().clone();
    assert_eq!(gen1.generation, 1);
    assert!(gen1.running);
    let pid1 = gen1.pid.ok_or("missing generation 1 pid")?;
    let addr1 = gen1.address.clone().ok_or("missing generation 1 address")?;

    // Generation 1 serves /health and executes P2_DEV_REBUILD_CMD on reload.
    assert!(health_ok(&addr1).await?);
    let reload = channel::send(dir.path(), &DevCommand::Reload { plugins: None }).await?;
    assert!(reload.ok, "{}", reload.message);
    assert_eq!(reload.plugins, Some(vec!["counter".to_owned()]));

    // Trigger coordinated supervisor restart over dev.sock.
    let restart = channel::send(dir.path(), &DevCommand::Restart).await?;
    assert!(restart.ok, "{}", restart.message);
    assert_eq!(restart.message, "Restarting server");

    let gen2 = timeout(Duration::from_secs(30), async {
        loop {
            let current = state.borrow_and_update().clone();
            if current.generation == 2 && current.running {
                return Ok::<_, tokio::sync::watch::error::RecvError>(current);
            }
            state.changed().await?;
        }
    })
    .await??;
    let pid2 = gen2.pid.ok_or("missing generation 2 pid")?;
    let addr2 = gen2.address.ok_or("missing generation 2 address")?;
    assert_ne!(pid1, pid2);
    assert!(health_ok(&addr2).await?);

    let after_restart = channel::send(dir.path(), &DevCommand::Reload { plugins: None }).await?;
    assert!(after_restart.ok, "{}", after_restart.message);

    supervisor.stop().await?;
    assert!(!dir.path().join("dev.sock").exists());
    Ok(())
}

async fn health_ok(url: &str) -> Result<bool, Box<dyn std::error::Error>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let host_port = url.strip_prefix("http://").unwrap_or(url);
    let mut stream = tokio::net::TcpStream::connect(host_port).await?;
    stream
        .write_all(
            format!("GET /health HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    Ok(response.starts_with("HTTP/1.1 200") && response.contains(r#""ok":true"#))
}

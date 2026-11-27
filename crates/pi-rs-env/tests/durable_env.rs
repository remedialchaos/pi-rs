//! Exercise the durable public interface against the compiled daemon over stdio.
use pi_rs_chord::context::Context;
use pi_rs_durable::env::*;
use pi_rs_env::{Connection, ConnectionOptions, RemoteExecutionEnv, RemoteExecutionEnvOptions};
use std::{collections::BTreeMap, sync::Arc};
use tempfile::TempDir;

fn adapter(cwd: &str) -> RemoteExecutionEnv {
    RemoteExecutionEnv::new(RemoteExecutionEnvOptions {
        connection: Arc::new(Connection::new(ConnectionOptions::new(vec![
            env!("CARGO_BIN_EXE_pi-env").to_owned(),
        ]))),
        cwd: cwd.into(),
        id: "pi-env:test".into(),
        shell_env: BTreeMap::from([("PI_ENV_TEST".into(), "configured".into())]),
        shell_path: None,
        watch: pi_rs_env::RemoteWatchOptions {
            mode: Some(pi_rs_env::WatchMode::Polling),
            poll_interval_ms: Some(25),
            max_directories: None,
        },
    })
}
type TestResult = Result<(), Box<dyn std::error::Error>>;
#[tokio::test]
async fn durable_readers_hold_open_files_and_both_adapters_list_filtered_entries() -> TestResult {
    let root = TempDir::new()?;
    let cwd = root.path().to_string_lossy();
    let remote = adapter(&cwd);
    let local = pi_rs_env::LocalExecutionEnv::new(
        LocalEnvOptions {
            cwd: cwd.to_string(),
            ..Default::default()
        },
        remote.connection().clone(),
    );
    let env: &dyn ExecutionEnv = &remote;
    let ctx = Context::background();
    let bytes = vec![0x61; 600_000];
    env.write_file("sub/a", &bytes, &ctx).await?;
    let mut reader = env.open_binary_reader("sub/a", None, &ctx).await?;
    env.rename_file("sub/a", "sub/z", &ctx).await?;
    env.write_file("sub/a", b"\xef\xbb\xbfnew\nlast", &ctx)
        .await?;
    env.append_file("sub/a", b"!", &ctx).await?;
    assert_eq!(reader.read(0, 700_000, &ctx).await?, bytes);
    assert_eq!(reader.info(&ctx).await?.size, 600_000);
    reader.close(&ctx).await;
    assert_eq!(
        reader.read(0, 0, &ctx).await.unwrap_err().code,
        FileErrorCode::Invalid
    );
    assert_eq!(
        env.read_text_lines("sub/a", None, &ctx).await?,
        ["new", "last!"]
    );
    assert!(
        env.read_text_file("sub/a", &ctx)
            .await?
            .starts_with('\u{feff}')
    );
    assert_eq!(
        env.list_dir("sub", &ctx)
            .await?
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        ["a", "z"]
    );
    let options = ListFilesOptions {
        exclude: vec!["z".into()],
        ..Default::default()
    };
    let expected = FileListing {
        entries: vec![
            FileListEntry {
                kind: FileKind::Directory,
                path: "sub".into(),
            },
            FileListEntry {
                kind: FileKind::File,
                path: "sub/a".into(),
            },
        ],
        truncated: false,
    };
    for env in [env, &local as &dyn ExecutionEnv] {
        assert_eq!(
            env.list_files(".", Some(options.clone()), &ctx).await?,
            expected
        );
    }
    assert!(!env.exists("missing", &ctx).await?);
    Ok(())
}
#[cfg(unix)]
#[tokio::test]
async fn shell_applies_config_routes_stderr_and_propagates_callback_failure() -> TestResult {
    let root = TempDir::new()?;
    let env = adapter(&root.path().to_string_lossy());
    let ctx = Context::background();
    let mut out = String::new();
    let mut err = String::new();
    let mut output = |text: &str, _: &Context, info: ShellOutputInfo| {
        match info.stream {
            ShellOutputStream::Stderr => err.push_str(text),
            ShellOutputStream::Stdout => out.push_str(text),
        };
        Ok(())
    };
    let result = env
        .exec(
            ShellCommand::Script("printf %s \"$PI_ENV_TEST\"; printf problem >&2; exit 7"),
            Some(ShellExecOptions {
                on_output: Some(&mut output),
                ..Default::default()
            }),
            &ctx,
        )
        .await?;
    assert_eq!(
        (result.exit_code, out.as_str(), err.as_str()),
        (7, "configured", "problem")
    );
    let mut reject = |_: &str, _: &Context, _: ShellOutputInfo| {
        Err(ExecutionError::new(ExecutionErrorCode::Unknown, "stop"))
    };
    let result = env
        .exec(
            ShellCommand::Argv(&["printf", "event"]),
            Some(ShellExecOptions {
                on_output: Some(&mut reject),
                ..Default::default()
            }),
            &ctx,
        )
        .await
        .unwrap_err();
    assert_eq!(
        (result.code, result.message.as_str()),
        (ExecutionErrorCode::CallbackError, "stop")
    );
    Ok(())
}

#[tokio::test]
async fn offline_listing_validates_limits_before_cancellation_or_connection() {
    let connection = Arc::new(Connection::new(
        ConnectionOptions::new(Vec::<String>::new()),
    ));
    let (ctx, cancel) = Context::background().with_cancel();
    cancel.cancel("offline");
    let invalid = ListFilesOptions {
        max_entries: Some(1 << 53),
        ..Default::default()
    };
    let error = pi_rs_env::list_files_over(&connection, "/offline", Some(invalid), &ctx)
        .await
        .unwrap_err();
    assert_eq!(error.code, FileErrorCode::Invalid);
    assert_eq!(error.path.as_deref(), Some("/offline"));
    assert_eq!(
        error.message,
        "maxEntries must be a non-negative safe integer"
    );
    let error = pi_rs_env::list_files_over(&connection, "/offline", None, &ctx)
        .await
        .unwrap_err();
    assert_eq!(error.code, FileErrorCode::Aborted);
}

#[tokio::test]
async fn durable_watcher_uses_configured_polling_and_reports_absolute_paths() -> TestResult {
    let root = TempDir::new()?;
    let env = adapter(&root.path().to_string_lossy());
    let ctx = Context::background();
    let (changes, mut received) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = env
        .watch(
            &[WatchTarget {
                path: ".".into(),
                ..Default::default()
            }],
            Box::new(move |event| {
                let _ = changes.send(event);
            }),
            &ctx,
        )
        .await?;
    assert_eq!(watcher.mode(), WatchMode::Polling);
    env.write_file("changed", b"event", &ctx).await?;
    let expected = root.path().join("changed").to_string_lossy().into_owned();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match received.recv().await {
                Some(FileChange::Paths { paths }) if paths.contains(&expected) => return Ok(()),
                Some(FileChange::Failed { error }) => return Err(error.message),
                None => return Err(String::from("watch channel closed")),
                _ => {}
            }
        }
    })
    .await??;
    watcher.stop(&ctx).await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn heartbeat_cancels_read_while_daemon_waits_for_fifo_data() -> TestResult {
    use std::{io::Write, time::Duration};
    let root = TempDir::new()?;
    let fifo = root.path().join("delayed");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()?
            .success()
    );
    let env = adapter(&root.path().to_string_lossy());
    let (ctx, cancel) = Context::background().with_cancel();
    let (opened, ready) = tokio::sync::oneshot::channel();
    let (release, heartbeat) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || -> std::io::Result<bool> {
        let mut pipe = std::fs::OpenOptions::new().write(true).open(fifo)?;
        let _ = opened.send(());
        let progressed = heartbeat.recv_timeout(Duration::from_secs(2)).is_ok();
        pipe.write_all(b"x")?;
        Ok(progressed)
    });
    let tick = async {
        ready.await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel("heartbeat");
        let _ = release.send(());
        Ok::<_, tokio::sync::oneshot::error::RecvError>(())
    };
    let (result, tick) = tokio::join!(env.read_binary_file("delayed", &ctx), tick);
    tick?;
    assert!(
        writer.join().map_err(|_| "FIFO writer panicked")??,
        "executor stalled while daemon waited"
    );
    assert_eq!(result.unwrap_err().code, FileErrorCode::Aborted);
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn chunked_write_returns_after_ordered_bytes_and_daemon_handle_close() -> TestResult {
    let dir = TempDir::new()?;
    let env = adapter(&dir.path().to_string_lossy());
    let ctx = Context::background();
    let data = (0u8..12)
        .flat_map(|n| std::iter::repeat_n(n, 512 * 1024))
        .collect::<Vec<_>>();
    let pid = env.connection().info()?.pid;
    let path = dir.path().join("written");
    env.write_file("written", &data, &ctx).await?;
    for fd in std::fs::read_dir(format!("/proc/{pid}/fd"))? {
        if let Ok(target) = std::fs::read_link(fd?.path()) {
            assert_ne!(target, path);
        }
    }
    assert!(std::fs::read(path)? == data);
    Ok(())
}

#[tokio::test]
async fn cancelled_join_returns_aborted_before_resolving_remote_paths() -> TestResult {
    let env = adapter("/");
    let (ctx, cancel) = Context::background().with_cancel();
    cancel.cancel("cancelled");
    assert_eq!(
        env.join_path(&["a", "b"], &ctx).await.err().map(|e| e.code),
        Some(FileErrorCode::Aborted)
    );
    Ok(())
}

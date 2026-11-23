//! Real end-to-end tests: spawn the compiled `pi-env` daemon and drive it through the Rust client transport.

use pi_rs_env::{
    AbortController, Connection, ConnectionOptions, FileChange, ProcessOptions, RemoteError,
    RemoteWatchOptions, RequestOptions, TerminalOptions, WatchMode, WatchTarget,
    is_connection_lost, local_daemon, node_platform, open_process, open_terminal,
};
use serde_json::{Value, json};
use std::fs;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;

fn daemon_path() -> String {
    local_daemon()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_pi-env").to_string())
}

fn connection() -> Arc<Connection> {
    Arc::new(Connection::new(ConnectionOptions::new(vec![daemon_path()])))
}

fn wait_for<T>(receiver: &Receiver<T>) -> T {
    receiver
        .recv_timeout(Duration::from_secs(15))
        .expect("the daemon did not answer in time")
}

#[test]
fn local_daemon_resolves_to_the_compiled_binary() {
    let resolved = local_daemon().unwrap();
    assert_eq!(
        resolved,
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi-env")),
    );
    let connection = Arc::new(Connection::new(ConnectionOptions::new(vec![
        resolved.to_string_lossy().into_owned(),
    ])));
    assert_eq!(connection.info().unwrap().protocol, 1);
}

#[test]
fn hello_reports_the_remote_machine_and_platform_mapping() {
    let connection = connection();
    let info = connection.info().unwrap();
    assert_eq!(info.protocol, 1);
    assert_eq!(info.version, "1.6.0");
    assert!(!info.os.is_empty() && !info.arch.is_empty() && !info.cwd.is_empty());
    assert!(info.has_feature("listFiles") && info.has_feature("process"));
    assert_eq!(
        node_platform(&info).0,
        if cfg!(windows) {
            "win32"
        } else {
            std::env::consts::OS
        }
    );
    assert!(!info.features.is_empty());
    connection.close();
    let error = connection.info().unwrap_err();
    assert_eq!(error.message, "Connection closed");
}

#[test]
fn filesystem_operations_and_node_style_error_codes() {
    let connection = connection();
    let directory = TempDir::new().unwrap();
    let root = directory.path().to_string_lossy().into_owned();
    let file = format!("{root}/sub/nested.txt");

    let missing = connection
        .request(
            "lstat",
            json!({ "path": format!("{root}/missing") }),
            RequestOptions::default(),
        )
        .unwrap_err();
    assert_eq!(missing.code, "ENOENT");

    let written = connection
        .request(
            "write",
            json!({ "path": file, "parents": true }),
            RequestOptions {
                payload: b"alpha\nbeta\n".to_vec(),
                ..RequestOptions::default()
            },
        )
        .unwrap();
    assert_eq!(written.json, json!({}));

    let opened = connection
        .request("open", json!({ "path": file }), RequestOptions::default())
        .unwrap();
    let handle = opened.json["handle"].as_u64().unwrap();
    assert_eq!(opened.json["info"]["kind"], "file");
    let read = connection
        .request(
            "pread",
            json!({ "handle": handle, "offset": 0, "length": 5 }),
            RequestOptions::default(),
        )
        .unwrap();
    assert_eq!(read.payload, b"alpha");
    let scanned = connection
        .request(
            "scanLines",
            json!({ "handle": handle, "startLine": 0 }),
            RequestOptions::default(),
        )
        .unwrap();
    assert_eq!(scanned.json["newlines"], 2);
    connection
        .request(
            "close",
            json!({ "handle": handle }),
            RequestOptions::default(),
        )
        .unwrap();

    connection
        .request(
            "mkdir",
            json!({ "path": format!("{root}/list"), "recursive": true }),
            RequestOptions::default(),
        )
        .unwrap();
    connection
        .request(
            "write",
            json!({ "path": format!("{root}/list/b.txt") }),
            RequestOptions {
                payload: b"b".to_vec(),
                ..RequestOptions::default()
            },
        )
        .unwrap();
    connection
        .request(
            "write",
            json!({ "path": format!("{root}/list/a.txt") }),
            RequestOptions {
                payload: b"a".to_vec(),
                ..RequestOptions::default()
            },
        )
        .unwrap();
    let listing = connection
        .request(
            "listFiles",
            json!({ "path": format!("{root}/list") }),
            RequestOptions::default(),
        )
        .unwrap();
    assert_eq!(listing.json["count"], 2);
    assert_eq!(listing.json["truncated"], false);
    assert_eq!(
        String::from_utf8_lossy(&listing.payload)
            .split('\0')
            .filter(|record| !record.is_empty())
            .collect::<Vec<_>>(),
        vec!["fa.txt", "fb.txt"]
    );
}

#[test]
fn exec_streams_output_spills_and_honours_cancellation() {
    let connection = connection();
    let directory = TempDir::new().unwrap();
    let cwd = directory.path().to_string_lossy().into_owned();
    let (events, received) = mpsc::channel::<(Value, Vec<u8>)>();
    let reply = connection
        .request(
            "exec",
            json!({ "command": "printf hello; printf warn >&2", "cwd": cwd, "env": {}, "inheritEnv": true }),
            RequestOptions {
                on_event: Some(Arc::new(move |event, payload| {
                    let _ = events.send((event.clone(), payload.to_vec()));
                })),
                ..RequestOptions::default()
            },
        )
        .unwrap();
    assert_eq!(reply.json["exitCode"], 0);
    let mut output = Vec::new();
    while let Ok((event, payload)) = received.try_recv() {
        if event["kind"] == "output" {
            output.push((event["stream"].as_str().unwrap_or("").to_string(), payload));
        }
    }
    assert!(output.iter().any(|(_, bytes)| bytes == b"hello"));
    assert!(output.iter().any(|(_, bytes)| bytes == b"warn"));

    // Output crossing the spill threshold leaves the complete raw output in a temporary file.
    let spilled = connection
        .request(
            "exec",
            json!({
                "command": "printf 0123456789",
                "cwd": cwd,
                "env": {},
                "spill": { "afterBytes": 3, "afterLines": 100 },
            }),
            RequestOptions::default(),
        )
        .unwrap();
    let spill_path = spilled.json["spillPath"].as_str().unwrap();
    assert_eq!(fs::read(spill_path).unwrap(), b"0123456789");

    // Aborting a long command sends a cancel and settles as `aborted`.
    let controller = AbortController::new();
    let signal = controller.signal();
    let cancelling = connection.clone();
    let cwd_clone = cwd.clone();
    let handle = std::thread::spawn(move || {
        cancelling.request(
            "exec",
            json!({ "command": "sleep 30", "cwd": cwd_clone, "env": {} }),
            RequestOptions {
                signal: Some(signal),
                ..RequestOptions::default()
            },
        )
    });
    std::thread::sleep(Duration::from_millis(300));
    controller.abort();
    let error = handle.join().unwrap().unwrap_err();
    assert_eq!(error.code, "aborted");
}

#[test]
fn remote_process_echoes_stdin_and_reports_stderr_and_exit() {
    let connection = connection();
    let directory = TempDir::new().unwrap();
    let (stdout, out) = mpsc::channel::<Vec<u8>>();
    let (stderr, err) = mpsc::channel::<Vec<u8>>();
    let process = open_process(
        Arc::clone(&connection),
        ProcessOptions {
            cwd: directory.path().to_string_lossy().into_owned(),
            argv: vec![
                "sh".to_string(),
                "-c".to_string(),
                "read line; printf 'out:%s' \"$line\"; printf err >&2; exit 4".to_string(),
            ],
            env: None,
            inherit_env: None,
            on_stdout: Arc::new(move |bytes| {
                let _ = stdout.send(bytes.to_vec());
            }),
            on_stderr: Some(Arc::new(move |bytes| {
                let _ = stderr.send(bytes.to_vec());
            })),
        },
    )
    .unwrap();
    assert!(process.pid > 0);
    process.write(b"hello\n");
    process.close_stdin();
    assert_eq!(process.wait().unwrap(), 4);
    assert_eq!(String::from_utf8_lossy(&wait_for(&out)), "out:hello");
    assert_eq!(String::from_utf8_lossy(&wait_for(&err)), "err");
}

#[cfg(unix)]
#[test]
fn remote_terminal_reads_writes_resizes_and_exits() {
    let connection = connection();
    let directory = TempDir::new().unwrap();
    let cwd = directory.path().to_string_lossy().into_owned();
    let output: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let writer = Arc::clone(&output);
    let terminal = open_terminal(
        Arc::clone(&connection),
        TerminalOptions {
            cwd: cwd.clone(),
            argv: Some(vec!["sh".to_string()]),
            env: None,
            inherit_env: None,
            cols: 80,
            rows: 24,
            on_data: Arc::new(move |bytes| writer.lock().unwrap().extend_from_slice(bytes)),
        },
    )
    .unwrap();
    assert!(terminal.pid > 0);
    terminal.write(b"echo TERMINAL_OK\n");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !String::from_utf8_lossy(&output.lock().unwrap()).contains("TERMINAL_OK") {
        assert!(
            std::time::Instant::now() < deadline,
            "the terminal did not echo the command"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    terminal.resize(100, 30);
    assert!(terminal.foreground().unwrap().is_none());
    terminal.close();
    assert_eq!(terminal.wait().unwrap_err().code, "aborted");

    let terminal = open_terminal(
        Arc::clone(&connection),
        TerminalOptions {
            cwd,
            argv: Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                "exit 7".to_string(),
            ]),
            env: None,
            inherit_env: None,
            cols: 40,
            rows: 12,
            on_data: Arc::new(|_| {}),
        },
    )
    .unwrap();
    assert_eq!(terminal.wait().unwrap(), 7);
}

#[cfg(unix)]
#[test]
fn remote_watcher_reports_created_modified_and_removed_files() {
    let connection = connection();
    let directory = TempDir::new().unwrap();
    let root = directory.path().to_string_lossy().into_owned();
    let file = format!("{root}/watched.txt");
    fs::write(&file, b"first").unwrap();
    let (changes, received) = mpsc::channel::<FileChange>();
    let watcher = pi_rs_env::RemoteWatcher::open(
        Arc::clone(&connection),
        vec![WatchTarget {
            path: root.clone(),
            recursive: false,
            exclude: None,
        }],
        Arc::new(move |change| {
            let _ = changes.send(change);
        }),
        RemoteWatchOptions {
            mode: Some(WatchMode::Native),
            ..RemoteWatchOptions::default()
        },
    )
    .unwrap();
    assert_eq!(watcher.mode(), WatchMode::Native);

    let mut seen = Vec::new();
    fs::write(&file, b"second").unwrap();
    fs::write(format!("{root}/created.txt"), b"new").unwrap();
    fs::remove_file(&file).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        while let Ok(change) = received.try_recv() {
            if let FileChange::Paths(paths) = change {
                seen.extend(paths);
            }
        }
        if seen.iter().any(|path| path.ends_with("watched.txt"))
            && seen.iter().any(|path| path.ends_with("created.txt"))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        seen.iter().any(|path| path.ends_with("watched.txt")),
        "changes: {seen:?}"
    );
    assert!(
        seen.iter().any(|path| path.ends_with("created.txt")),
        "changes: {seen:?}"
    );
    watcher.stop();
}

#[test]
fn a_lost_connection_fails_in_flight_requests_and_the_next_one_restarts_the_daemon() {
    let connection = connection();
    let pid = connection.info().unwrap().pid;
    // A request already in flight when the daemon dies fails as connection-lost; a fresh one restarts the daemon.
    let (started_sender, started_receiver) = mpsc::channel::<()>();
    let started_sender = Arc::new(Mutex::new(started_sender));
    let in_flight = Arc::clone(&connection);
    let handle = std::thread::spawn(move || {
        let signal = Arc::clone(&started_sender);
        in_flight.request(
            "exec",
            json!({ "command": "sleep 10", "cwd": "/", "env": {} }),
            RequestOptions {
                on_start: Some(Arc::new(move |_, _| {
                    let _ = signal.lock().unwrap().send(());
                })),
                ..RequestOptions::default()
            },
        )
    });
    wait_for(&started_receiver);
    let killed = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .unwrap();
    assert!(killed.success());

    let error: RemoteError = handle.join().unwrap().unwrap_err();
    assert!(is_connection_lost(&error), "unexpected error: {error:?}");

    let restarted = connection.info().unwrap();
    assert_ne!(restarted.pid, pid);
    assert_eq!(restarted.protocol, 1);
}

use pi_rs_p2::runtime::logs::{LogLevel, LogStore, ProcessCapture, Publication};
use std::{io::Write, process::Command};
#[test]
fn capture_persists_redacted_utf8_after_eviction_and_teardown()
-> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("P2_LOG_FIXTURE").is_none() {
        let status = Command::new(std::env::current_exe()?)
            .env_clear()
            .env("P2_LOG_FIXTURE", "1")
            .args([
                "--exact",
                "capture_persists_redacted_utf8_after_eviction_and_teardown",
            ])
            .status()?;
        assert!(status.success());
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("owner-key"), "fixture-literal")?;
    let path = dir.path().join("dev.logs.json");
    let store = LogStore::default();
    store.add_directory(dir.path());
    let started = std::time::Instant::now();
    let publisher = Publication::start(store.clone(), "dev".into(), path.clone())?;
    store.append_at("dev", LogLevel::Event, &"noise\n".repeat(1000), 1);
    let mut output = Vec::new();
    {
        let lease = ProcessCapture::acquire(store.clone(), "dev")?;
        let mut capture = lease.stderr(&mut output);
        for byte in "é fixture-literal\r\nlast".bytes() {
            capture.write_all(&[byte])?;
        }
    }
    drop(publisher);
    assert_eq!(output, "é fixture-literal\r\nlast".as_bytes());
    let snapshot = std::fs::read_to_string(path)?;
    assert!(!snapshot.contains("fixture-literal"));
    let parsed: serde_json::Value = serde_json::from_str(&snapshot)?;
    let lines = parsed["lines"].as_array().ok_or("missing lines")?;
    eprintln!("persisted fixture in {:?}", started.elapsed());
    assert_eq!(lines.len(), 1000);
    assert_eq!(lines[0]["id"], 4);
    assert_eq!(lines[998]["text"], "é [redacted]");
    assert_eq!(lines[999]["text"], "last");
    let (tx, rx) = std::sync::mpsc::channel();
    let watched = store.clone();
    let id = store.subscribe(std::sync::Arc::new(move |source| {
        let _ = tx.send((source.to_owned(), watched.notices()));
    }));
    store.notice("server", "first");
    store.notice("server", "fixture-literal");
    let (_, first) = rx.try_recv()?;
    let (source, next) = rx.try_recv()?;
    assert_eq!(source, "server");
    assert_eq!(first.len(), 1);
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].message, "[redacted]");
    store.dismiss_notice(first[0].id);
    assert!(rx.try_recv().is_err());
    store.dismiss_notice(next[0].id);
    assert!(rx.try_recv()?.1.is_empty());
    store.unsubscribe(id);
    store.notice("server", "later");
    assert!(rx.try_recv().is_err());
    Ok(())
}

#[test]
fn host_failure_publishes_shared_capture_after_overflow() -> Result<(), Box<dyn std::error::Error>>
{
    use pi_rs_p2::runtime::logs::PublishedLogs;
    if let Some(dir) = std::env::var_os("P2_FATAL_FIXTURE") {
        let store = LogStore::default();
        let directory = std::path::Path::new(&dir);
        let logs = PublishedLogs::start(store.clone(), "dev", directory)?;
        let result = logs.run(|capture| -> std::io::Result<()> {
            let mut output = capture.stdout(std::io::stdout());
            output.write_all(&vec![b'x'; 262145])?;
            let second = ProcessCapture::acquire(store, "dev")?;
            let mut alias = second.stdout(std::io::stdout());
            alias.write_all(b"ignored\njoined-")?;
            drop(alias);
            drop(second);
            output.write_all(b"tail")?;
            output.flush()?;
            Err(std::io::Error::other("fatal fixture-secret"))
        });
        if result.is_err() {
            std::process::exit(17)
        }
        return Err("expected failure".into());
    }
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("owner-key"), "fixture-secret")?;
    let status = Command::new(std::env::current_exe()?)
        .env_clear()
        .env("P2_FATAL_FIXTURE", dir.path())
        .args([
            "--exact",
            "host_failure_publishes_shared_capture_after_overflow",
        ])
        .output()?;
    assert_eq!(status.status.code(), Some(17));
    assert!(status.stdout.ends_with(b"ignored\njoined-tail"));
    assert!(
        status
            .stdout
            .windows(262145)
            .any(|bytes| bytes.iter().all(|b| *b == b'x'))
    );
    let snapshot = std::fs::read_to_string(dir.path().join("dev.logs.json"))?;
    let parsed: serde_json::Value = serde_json::from_str(&snapshot)?;
    let lines = parsed["lines"].as_array().ok_or("missing lines")?;
    let stdout: Vec<_> = lines
        .iter()
        .filter(|v| v["level"] == "stdout")
        .map(|v| v["text"].as_str())
        .collect();
    assert_eq!(
        stdout,
        [Some("[oversized log line omitted]"), Some("joined-tail")]
    );
    assert!(
        lines
            .iter()
            .any(|v| v["level"] == "error" && v["text"] == "fatal [redacted]")
    );
    Ok(())
}

#[tokio::test]
async fn standalone_hosts_publish_before_exit_and_keep_worker_stdout_framed()
-> Result<(), Box<dyn std::error::Error>> {
    use pi_rs_chord::node::workers::{Control, Frame, read_frame, write_frame};
    use serde_json::{Value, json};
    use std::process::Stdio;
    let dir = tempfile::tempdir()?;
    let host = || {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_p2"));
        cmd.env_clear()
            .env("P2_DATA_DIR", dir.path())
            .env("HOME", dir.path())
            .current_dir(dir.path())
            .kill_on_drop(true);
        cmd
    };
    let snapshot = |name: &str| -> Result<Value, Box<dyn std::error::Error>> {
        Ok(serde_json::from_slice(&std::fs::read(
            dir.path().join(name),
        )?)?)
    };
    let server = host().env("P2_PROVIDER", "fixture").output().await?;
    assert_eq!(server.status.code(), Some(1));
    let lines = snapshot("server.logs.json")?;
    assert_eq!(lines["lines"][0]["text"], "Starting");
    assert_eq!(lines["lines"][1]["level"], "error");
    assert_eq!(
        lines["lines"][1]["text"],
        "Set both P2_PROVIDER and P2_MODEL"
    );

    #[cfg(unix)]
    {
        let mut server = host().env("PORT", "0").stdout(Stdio::piped()).spawn()?;
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut banner = String::new();
            BufReader::new(server.stdout.take().ok_or("missing server stdout")?)
                .read_line(&mut banner)
                .await?;
            assert!(banner.starts_with("p2: http://"));
            loop {
                let live = snapshot("server.logs.json")?;
                if live["lines"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().any(|l| l["text"] == banner.trim_end()))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let pid = server.id().ok_or("missing server pid")?;
            assert!(
                Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .status()?
                    .success()
            );
            assert!(server.wait().await?.success());
            Ok::<(), Box<dyn std::error::Error>>(())
        })
        .await??;
        let closed = snapshot("server.logs.json")?;
        assert_eq!(
            closed["lines"]
                .as_array()
                .and_then(|rows| rows.last())
                .map(|l| &l["text"]),
            Some(&json!("Closed"))
        );
    }
    let bad = host().arg("--worker").stdin(Stdio::null()).output().await?;
    assert_eq!(bad.status.code(), Some(1));
    assert!(bad.stdout.is_empty());
    assert_eq!(
        snapshot("worker-startup.logs.json")?["lines"][1]["level"],
        "error"
    );

    let mut worker = host()
        .arg("--worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut stdin = worker.stdin.take().ok_or("missing stdin")?;
    let mut stdout = worker.stdout.take().ok_or("missing stdout")?;
    tokio::time::timeout(std::time::Duration::from_secs(30),async{
 let start=json!({"app":dir.path(),"dataDir":dir.path(),"fake":true,"telemetry":false,"generation":1,"values":{"session":{"id":"active","cwd":dir.path(),"model":{"provider":"faux","modelId":"faux-1"},"thinkingLevel":"off"}}});
 write_frame(&mut stdin,&Frame::Control(Control::Start{address:pi_rs_p2::api::v1::session_address("active"),start,scripts:vec![],lock:dir.path().join("lock")})).await?;
 assert!(matches!(read_frame(&mut stdout).await?,Frame::Control(Control::Started)));
 loop {if matches!(read_frame(&mut stdout).await?,Frame::Control(Control::Ready{..})){break}}
 write_frame(&mut stdin,&Frame::Control(Control::Shutdown)).await?;drop(stdin);
 use tokio::io::AsyncReadExt;
 let mut tail=Vec::new();stdout.read_to_end(&mut tail).await?;
 let mut bytes=&tail[..];while !bytes.is_empty(){read_frame(&mut bytes).await?;}
 assert!(worker.wait().await?.success());
 Ok::<(),Box<dyn std::error::Error>>(())
 }).await??;
    let session = snapshot("sessions/active/worker.logs.json")?;
    assert_eq!(session["source"], "session:active");
    assert_eq!(session["lines"][0]["text"], "Starting");
    assert_eq!(session["lines"][1]["text"], "Closed");
    Ok(())
}

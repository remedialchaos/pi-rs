//! The real `p3` binary, as a user runs it: the command line's answers, a Pi started in the background
//! and in the foreground on a temporary data directory, its owner link over HTTP, and its stop.

use std::{
    ffi::OsString,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use pi_rs_p3::client::{self, Request};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_p3"))
}

/// Runs `p3` with `args` and the test's home, and answers its exit code and output.
fn run(home: &Path, args: &[&str]) -> (i32, String) {
    let output = Command::new(binary())
        .args(args)
        .env("HOME", home)
        .output()
        .expect("the p3 binary runs");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code().unwrap_or(-1), text)
}

fn home() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

fn port_of(text: &str) -> Option<(u16, String)> {
    let rest = text.split("port ").nth(1)?;
    let port = rest.split('.').next()?.trim().parse::<u16>().ok()?;
    Some((port, text.to_string()))
}

#[test]
fn help_and_version_answer_without_a_pi() {
    let home = home();
    let (code, text) = run(home.path(), &["--help"]);
    assert_eq!(code, 0);
    assert!(text.contains("Usage: p3"), "{text}");
    assert!(text.contains("server     Run the Pi's server"), "{text}");
    let (code, text) = run(home.path(), &["--version"]);
    assert_eq!(code, 0);
    assert_eq!(text.trim(), env!("CARGO_PKG_VERSION"));
}

#[test]
fn a_command_line_it_does_not_understand_exits_two() {
    let home = home();
    for args in [
        vec!["bogus"],
        vec!["status", "--port", "1"],
        vec!["--pi", "Not A Name"],
        vec!["server", "--pi", "one", "--dir", "/tmp/one"],
        vec!["list", "--pi", "two"],
    ] {
        let (code, text) = run(home.path(), &args);
        assert_eq!(code, 2, "{args:?}: {text}");
        assert!(text.contains("Usage: p3"), "{text}");
    }
}

#[test]
fn a_pi_that_never_started_is_not_running_and_is_not_listed() {
    let home = home();
    let dir = home.path().join("pi");
    let (code, text) = run(
        home.path(),
        &["status", "--dir", dir.to_str().expect("path")],
    );
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("not running"), "{text}");
    assert!(
        text.contains(&format!("data directory  {}", dir.display())),
        "{text}"
    );
    let (code, text) = run(home.path(), &["list"]);
    assert_eq!(code, 0);
    assert_eq!(text.trim(), "no Pi yet: `p3` starts the default one");
}

#[test]
fn a_background_pi_starts_once_prints_an_owner_link_and_stops() {
    let home = home();
    let dir = home.path().join("pi");
    let dir_text = dir.to_str().expect("path").to_string();
    let _cleanup = Cleanup(dir.clone());
    let (code, first) = run(home.path(), &["--dir", &dir_text, "--port", "0"]);
    assert_eq!(code, 0, "{first}");
    let (port, _) = port_of(&first).expect("a port");
    assert!(
        first.contains(&format!("http://127.0.0.1:{port}/_p3/owner#")),
        "{first}"
    );

    // The server's files: its record, its port, and its owner token file.
    assert!(dir.join("running.json").is_file());
    assert!(dir.join("owner-token").is_file(), "the owner token file");
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("running.json")).expect("record"))
            .expect("record JSON");
    assert_eq!(record["port"], serde_json::json!(port));

    // A second start finds it running: the same port, a fresh link.
    let (code, second) = run(home.path(), &["--dir", &dir_text]);
    assert_eq!(code, 0, "{second}");
    assert!(second.contains(&format!("port {port}.")), "{second}");
    assert_ne!(second, first);

    // Its status names what it is.
    let (code, status) = run(home.path(), &["status", "--dir", &dir_text]);
    assert_eq!(code, 0, "{status}");
    assert!(status.starts_with("running"), "{status}");
    assert!(
        status.contains(&format!("port            {port}")),
        "{status}"
    );
    assert!(status.contains("version         0."), "{status}");
    assert!(status.contains("started         20"), "{status}");

    // The owner link signs a browser in once. The second start's link is the fresh one: a new link
    // replaces the previous one.
    let link = second.split("Open ").nth(1).expect("a link").trim();
    let token = link.split('#').nth(1).expect("a token");
    let signed_in = client::post(
        port,
        "/_p3/owner",
        Request {
            json: Some(&serde_json::json!({ "token": token }).to_string()),
            ..Request::default()
        },
    )
    .expect("the owner page's post");
    assert_eq!(signed_in.status, 200, "{:?}", signed_in.body);
    let cookie = signed_in
        .header("set-cookie")
        .expect("a sign-in cookie")
        .split(';')
        .next()
        .expect("a cookie pair")
        .to_string();
    // The cookie asks for a fresh link; the used link is refused.
    let with_cookie = client::post(
        port,
        "/_p3/owner/link",
        Request {
            cookie: Some(&cookie),
            ..Request::default()
        },
    )
    .expect("the owner link route");
    assert_eq!(with_cookie.status, 200);
    let used = client::post(
        port,
        "/_p3/owner",
        Request {
            json: Some(&serde_json::json!({ "token": token }).to_string()),
            ..Request::default()
        },
    )
    .expect("the owner page's post");
    assert_eq!(used.status, 403);
    let anonymous = client::post(port, "/_p3/owner/link", Request::default()).expect("the route");
    assert_eq!(anonymous.status, 401);
    let stop_anonymous = client::post(port, "/_p3/stop", Request::default()).expect("the route");
    assert_eq!(stop_anonymous.status, 401);

    let (code, stopped) = run(home.path(), &["stop", "--dir", &dir_text]);
    assert_eq!(code, 0, "{stopped}");
    assert_eq!(stopped.trim(), "stopped");
    let (code, after) = run(home.path(), &["status", "--dir", &dir_text]);
    assert_eq!(code, 1, "{after}");
    assert!(after.contains("not running"), "{after}");
}

#[test]
fn a_named_pi_keeps_its_port_and_is_listed_and_restarted() {
    let home = home();
    let (code, started) = run(home.path(), &["--pi", "two"]);
    assert_eq!(code, 0, "{started}");
    let (port, _) = port_of(&started).expect("a port");
    let kept: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join(".pi/p3/pis/two/pi.json")).expect("pi.json"),
    )
    .expect("pi.json JSON");
    assert_eq!(kept["port"], serde_json::json!(port));

    let (code, list) = run(home.path(), &["list"]);
    assert_eq!(code, 0);
    assert!(
        list.contains("two") && list.contains("running") && list.contains(&port.to_string()),
        "{list}"
    );

    let (code, restarted) = run(home.path(), &["restart", "--pi", "two"]);
    assert_eq!(code, 0, "{restarted}");
    assert!(restarted.contains(&format!("port {port}.")), "{restarted}");

    let (code, stopped) = run(home.path(), &["stop", "--pi", "two"]);
    assert_eq!(code, 0, "{stopped}");
    let (code, list) = run(home.path(), &["list"]);
    assert_eq!(code, 0);
    assert!(
        list.contains("two") && list.contains("stopped") && list.contains(&port.to_string()),
        "{list}"
    );
}

#[test]
fn a_foreground_server_runs_until_the_owner_stops_it() {
    let home = home();
    let dir = home.path().join("pi");
    let _cleanup = Cleanup(dir.clone());
    let dir_text = dir.to_str().expect("path").to_string();
    let mut child = Command::new(binary())
        .args(["server", "--dir", &dir_text, "--port", "0"])
        .env("HOME", home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the server starts");
    let line = first_line(&mut child);
    let port = port_of(&line).map(|(port, _)| port).expect("a port");
    assert!(
        line.contains(&format!("Open http://127.0.0.1:{port}/_p3/owner#")),
        "{line}"
    );
    let (code, status) = run(home.path(), &["status", "--dir", &dir_text]);
    assert_eq!(code, 0, "{status}");
    assert!(
        status.contains(&format!("port            {port}")),
        "{status}"
    );
    let (code, stopped) = run(home.path(), &["stop", "--dir", &dir_text]);
    assert_eq!(code, 0, "{stopped}");
    let status = wait_exit(&mut child, Duration::from_secs(10));
    assert_eq!(status, Some(0), "the foreground server exits cleanly");
}

fn first_line(child: &mut Child) -> String {
    let stdout = child.stdout.take().expect("stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        if reader.read_line(&mut line).is_ok() {
            let _ = sender.send(line);
        }
    });
    receiver
        .recv_timeout(Duration::from_secs(20))
        .expect("the server says what it started as")
        .trim()
        .to_string()
}

fn wait_exit(child: &mut Child, timeout: Duration) -> Option<i32> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status.code().unwrap_or(-1));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    None
}

/// Kills any p3 the test left behind: a failure in an assertion above must not leak a server.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = Command::new(binary())
            .args([
                OsString::from("stop").as_os_str(),
                OsString::from("--dir").as_os_str(),
                self.0.as_os_str(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

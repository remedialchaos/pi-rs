//! Whole-screen TUI visual parity tests against upstream `p2 tui` and `vt100` cell-grid geometry checks.

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use pi_rs_chord::{context::Context, host::Caller, node::workers::WorkerOptions};
use pi_rs_p2::{
    api::v1::p2_contracts::lookup,
    hosts::server::{P2Server, ServerOptions, start_server},
    plugins,
};
use pi_rs_tui::visible_width;
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

struct TmuxSession {
    name: String,
}

impl TmuxSession {
    fn spawn(name: &str, width: u16, height: u16, command: &str) -> TestResult<Self> {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let status = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                name,
                "-x",
                &width.to_string(),
                "-y",
                &height.to_string(),
                command,
            ])
            .status()?;
        if !status.success() {
            return Err(format!("tmux new-session failed for {name}").into());
        }
        Ok(Self {
            name: name.to_owned(),
        })
    }

    fn resize(&self, width: u16, height: u16) -> TestResult {
        let status = Command::new("tmux")
            .args([
                "resize-window",
                "-t",
                &self.name,
                "-x",
                &width.to_string(),
                "-y",
                &height.to_string(),
            ])
            .status()?;
        if !status.success() {
            return Err(format!("tmux resize-window failed for {}", self.name).into());
        }
        Ok(())
    }

    fn send_literal(&self, text: &str) -> TestResult {
        let status = Command::new("tmux")
            .args(["send-keys", "-t", &self.name, "-l", text])
            .status()?;
        if !status.success() {
            return Err(format!("tmux send-keys -l failed for {}", self.name).into());
        }
        Ok(())
    }

    fn send_key(&self, key: &str) -> TestResult {
        let status = Command::new("tmux")
            .args(["send-keys", "-t", &self.name, key])
            .status()?;
        if !status.success() {
            return Err(format!("tmux send-keys {key} failed for {}", self.name).into());
        }
        Ok(())
    }

    fn capture(&self, ansi: bool) -> TestResult<String> {
        let mut args = vec!["capture-pane", "-t", &self.name];
        if ansi {
            args.push("-e");
        }
        args.push("-p");
        let output = Command::new("tmux").args(&args).output()?;
        if !output.status.success() {
            return Err(format!("tmux capture-pane failed for {}", self.name).into());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn wait_for(&self, needle: &str, timeout: Duration) -> TestResult<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let plain = self.capture(false)?;
            if plain.contains(needle) {
                return Ok(plain);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for {needle:?} in tmux {}.\nLast screen:\n{plain}",
                    self.name
                )
                .into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for TmuxSession {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn require_tool(name: &str) -> TestResult {
    let status = Command::new(name)
        .arg(if name == "tmux" { "-V" } else { "--version" })
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(format!("{name} is required").into());
    }
    Ok(())
}

fn find_upstream_p2_dir() -> TestResult<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let worktree_root = manifest_dir
        .ancestors()
        .nth(3)
        .ok_or("cannot locate workspace root")?;
    for candidate in [
        worktree_root.to_path_buf(),
        worktree_root
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_default(),
    ] {
        if candidate.join("node_modules").exists()
            && candidate
                .join("packages/ai/src/providers/data/amazon-bedrock.json")
                .exists()
            && candidate.join("apps/p2/src/hosts/tui.ts").exists()
        {
            return Ok(candidate.join("apps/p2"));
        }
    }
    Err("cannot locate hydrated upstream apps/p2 checkout".into())
}

fn parse_vt100_rows(ansi_capture: &str, rows: u16, cols: u16) -> vt100::Screen {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    let normalized = ansi_capture.trim_end_matches('\n').replace('\n', "\r\n");
    parser.process(normalized.as_bytes());
    parser.screen().clone()
}

fn assert_frame_bounds(plain: &str, expected_rows: usize, max_cols: usize) -> Vec<String> {
    let mut lines: Vec<String> = plain.lines().map(str::to_owned).collect();
    while lines.len() < expected_rows {
        lines.push(String::new());
    }
    assert_eq!(
        lines.len(),
        expected_rows,
        "expected {expected_rows} rows, got {}:\n{plain}",
        lines.len()
    );
    for (idx, line) in lines.iter().enumerate() {
        let w = visible_width(line);
        assert!(
            w <= max_cols,
            "row {idx} overflowed {max_cols} columns (width={w}): {line:?}"
        );
    }
    lines
}

fn owner_caller() -> Caller {
    Caller {
        principal: Some(json!({"kind": "owner", "access": "local"})),
        incarnation: uuid::Uuid::new_v4().to_string(),
    }
}

async fn start_native_server(directory: &Path) -> TestResult<P2Server> {
    let mut options = ServerOptions::new(directory.to_owned());
    options.port = 0;
    options.fake = true;
    options.dev = false;
    plugins::register_builtins(&mut options.registry)?;
    let mut process = WorkerOptions::new(
        env!("CARGO_BIN_EXE_p2").into(),
        directory.join("worker-locks"),
    );
    process.args.push("--worker".into());
    options.worker_process = Some(process);
    Ok(start_server(options).await?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_upstream_vs_native_whole_screen_100x24_parity() -> TestResult {
    require_tool("tmux")?;
    require_tool("node")?;
    let upstream_p2 = find_upstream_p2_dir()?;

    let dir = tempfile::tempdir()?;
    let up_data = dir.path().join("up-data");
    let up_cfg = dir.path().join("up-cfg");
    let up_home = dir.path().join("up-home");
    let nat_data = dir.path().join("nat-data");
    let nat_cfg = dir.path().join("nat-cfg");
    let nat_home = dir.path().join("nat-home");
    for p in [&up_data, &up_cfg, &up_home, &nat_data, &nat_cfg, &nat_home] {
        fs::create_dir_all(p)?;
    }

    // 1. Start upstream disposable server (`P2_FAKE=1`).
    let up_log = dir.path().join("up-server.log");
    let up_log_file = fs::File::create(&up_log)?;
    let up_log_err = up_log_file.try_clone()?;
    let mut up_server = ChildGuard(
        Command::new("node")
            .args(["--import", "./src/source.ts", "src/hosts/server.ts"])
            .current_dir(&upstream_p2)
            .env("P2_FAKE", "1")
            .env("P2_DATA_DIR", &up_data)
            .env("PORT", "0")
            .env("HOME", &up_home)
            .stdout(Stdio::from(up_log_file))
            .stderr(Stdio::from(up_log_err))
            .spawn()?,
    );
    let up_url = {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            let text = fs::read_to_string(&up_log).unwrap_or_default();
            if let Some(pos) = text.find("http://127.0.0.1:") {
                let rest = &text[pos..];
                let end = rest.find([' ', '\n', '\r']).unwrap_or(rest.len());
                break rest[..end].to_owned();
            }
            if let Some(status) = up_server.0.try_wait()? {
                return Err(format!(
                    "upstream server exited early ({status}) in {}:\n{text}",
                    upstream_p2.display()
                )
                .into());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "upstream server did not start in {}:\n{text}",
                    upstream_p2.display()
                )
                .into());
            }
            thread::sleep(Duration::from_millis(100));
        }
    };

    // 2. Drive upstream `p2 tui` in a 100x24 tmux PTY.
    let up_cmd = format!(
        "cd '{}' && env HOME='{}' P2_DATA_DIR='{}' P2_TUI_CONFIG_DIR='{}' TERM=xterm-256color node --import ./src/source.ts src/hosts/tui.ts '{}' --theme dark",
        upstream_p2.display(),
        up_home.display(),
        up_data.display(),
        up_cfg.display(),
        up_url
    );
    let up_tmux = TmuxSession::spawn("p2-parity-up", 100, 24, &up_cmd)?;
    up_tmux.wait_for("New session", Duration::from_secs(15))?;
    up_tmux.send_key("Enter")?;
    up_tmux.wait_for("enter creates", Duration::from_secs(10))?;
    up_tmux.send_key("Enter")?;
    up_tmux.wait_for("(faux)", Duration::from_secs(10))?;

    let prompt = "Please explain how wrapping works in this terminal layout with a long paragraph that definitely exceeds eighty and one hundred columns so we can inspect wrapping.";
    up_tmux.send_literal(prompt)?;
    up_tmux.send_key("Enter")?;
    up_tmux.wait_for("Echo:", Duration::from_secs(15))?;
    thread::sleep(Duration::from_millis(300));
    let up_plain = up_tmux.capture(false)?;
    let up_ansi = up_tmux.capture(true)?;
    let up_rows = assert_frame_bounds(&up_plain, 24, 100);

    // 3. Start native disposable server + session + `p2 tui` in an identical 100x24 tmux PTY.
    let server = start_native_server(&nat_data).await?;
    let context = Context::background();
    let sessions = server.host.hold_as(
        Arc::new(lookup("p2.sessions")?),
        json!({}).into(),
        owner_caller(),
        &context,
    )?;
    sessions.wait(&context).await?;
    let _created: Value = sessions
        .call(
            "create",
            json!({
                "cwd": &nat_home,
                "title": "Parity Session",
                "thinkingLevel": "off",
                "model": {"provider": "faux", "modelId": "faux-1"},
            })
            .into(),
            &context,
        )
        .await?
        .to_serde()?;
    let nat_url = format!("http://{}", server.address);
    let nat_cmd = format!(
        "env HOME='{}' P2_DATA_DIR='{}' P2_TUI_CONFIG_DIR='{}' TERM=xterm-256color '{}' tui '{}' --theme dark",
        nat_home.display(),
        nat_data.display(),
        nat_cfg.display(),
        env!("CARGO_BIN_EXE_p2"),
        nat_url
    );
    let nat_tmux = TmuxSession::spawn("p2-parity-nat", 100, 24, &nat_cmd)?;
    nat_tmux.wait_for("Parity Session", Duration::from_secs(15))?;
    nat_tmux.send_literal(prompt)?;
    nat_tmux.send_key("Enter")?;
    nat_tmux.wait_for("Echo:", Duration::from_secs(15))?;
    thread::sleep(Duration::from_millis(300));
    let nat_plain = nat_tmux.capture(false)?;
    let nat_ansi = nat_tmux.capture(true)?;
    let nat_rows = assert_frame_bounds(&nat_plain, 24, 100);

    // 4. Parse both ANSI captures with `vt100` and compare structural whole-screen parity.
    let up_screen = parse_vt100_rows(&up_ansi, 24, 100);
    let nat_screen = parse_vt100_rows(&nat_ansi, 24, 100);
    assert_eq!(up_screen.size(), (24, 100));
    assert_eq!(nat_screen.size(), (24, 100));

    // Header row presence & alignment (logo + origin · owner + help prompt).
    for (label, rows, plain) in [
        ("upstream", &up_rows, &up_plain),
        ("native", &nat_rows, &nat_plain),
    ] {
        assert!(
            rows[1].contains("▀▀█") && rows[1].contains("owner"),
            "{label} header row 1 missing logo/owner:\n{plain}"
        );
        assert!(
            rows[2].contains("█▀ █") && rows[2].contains("interrupt"),
            "{label} header row 2 missing logo/hints:\n{plain}"
        );
        assert!(
            rows[3].contains("Press ctrl+o to show full startup help."),
            "{label} header row 3 missing startup help hint:\n{plain}"
        );
        // Transcript order & wrapped long paragraph.
        let user_pos = plain
            .find("Please explain how wrapping works")
            .ok_or_else(|| format!("{label} missing user message"))?;
        let echo_pos = plain
            .find("Echo:")
            .ok_or_else(|| format!("{label} missing assistant Echo"))?;
        assert!(
            user_pos < echo_pos,
            "{label} user message should precede Echo"
        );
        assert!(
            plain.contains("exceeds eighty and one hundred columns so we can inspect wrapping."),
            "{label} missing wrapped second line of user message:\n{plain}"
        );
        // Editor borders anchored near bottom.
        let border_rows: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.contains(&"─".repeat(80)))
            .map(|(i, _)| i)
            .collect();
        assert!(
            border_rows.len() >= 2 && *border_rows.last().unwrap_or(&0) >= 19,
            "{label} editor borders missing near bottom: {border_rows:?}\n{plain}"
        );
    }

    // Dark-theme RGB ANSI color presence on both upstream and native.
    for (label, ansi, rows, screen) in [
        ("upstream", &up_ansi, &up_rows, &up_screen),
        ("native", &nat_ansi, &nat_rows, &nat_screen),
    ] {
        assert!(
            ansi.contains("38;2;228;138;122") && ansi.contains("48;2;79;142;179"),
            "{label} missing Pi logo RGB ANSI colors"
        );
        assert!(
            ansi.contains("48;2;33;59;73") && ansi.contains("38;2;222;224;225"),
            "{label} missing dark theme userMessageBg/userMessageText ANSI colors"
        );
        let border_row = rows
            .iter()
            .position(|r| r.contains(&"─".repeat(80)))
            .ok_or_else(|| format!("{label} missing editor border row"))?;
        let border_fg = screen
            .cell(border_row as u16, 0)
            .ok_or("missing border cell")?
            .fgcolor();
        assert!(
            matches!(border_fg, vt100::Color::Rgb(r, g, b) if r < 140 && g < 145 && b < 150),
            "{label} unexpected dark theme borderMuted fg color: {border_fg:?}"
        );
    }

    sessions.release();
    server.close().await?;
    Ok(())
}

use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn side_panel_3_to_2_split_and_resize_cell_grid_parity() -> TestResult {
    require_tool("tmux")?;

    let dir = tempfile::tempdir()?;
    let home_dir = dir.path().join("home");
    let cfg_dir = dir.path().join("cfg");
    fs::create_dir_all(&home_dir)?;
    fs::create_dir_all(&cfg_dir)?;

    let server = start_native_server(dir.path()).await?;
    let context = Context::background();
    let sessions = server.host.hold_as(
        Arc::new(lookup("p2.sessions")?),
        json!({}).into(),
        owner_caller(),
        &context,
    )?;
    sessions.wait(&context).await?;
    let _created: Value = sessions
        .call(
            "create",
            json!({
                "cwd": &home_dir,
                "title": "Split Geometry",
                "thinkingLevel": "off",
                "model": {"provider": "faux", "modelId": "faux-1"},
            })
            .into(),
            &context,
        )
        .await?
        .to_serde()?;

    let url = format!("http://{}", server.address);
    let cmd = format!(
        "env HOME='{}' P2_DATA_DIR='{}' P2_TUI_CONFIG_DIR='{}' TERM=xterm-256color '{}' tui '{}' --theme dark",
        home_dir.display(),
        dir.path().display(),
        cfg_dir.display(),
        env!("CARGO_BIN_EXE_p2"),
        url
    );
    let tmux = TmuxSession::spawn("p2-parity-split", 120, 24, &cmd)?;
    tmux.wait_for("Split Geometry", Duration::from_secs(15))?;

    tmux.send_literal("/help")?;
    tmux.send_key("Enter")?;
    tmux.wait_for("Built-in Commands", Duration::from_secs(10))?;
    thread::sleep(Duration::from_millis(250));

    // Verify 120x24: header pinned at top, editor/footer pinned at bottom, continuous `│` divider across all body rows.
    let ansi_120 = tmux.capture(true)?;
    let plain_120 = tmux.capture(false)?;
    let rows_120 = assert_frame_bounds(&plain_120, 24, 120);
    let screen_120 = parse_vt100_rows(&ansi_120, 24, 120);
    assert!(rows_120[1].contains("▀▀█"), "header missing at 120x24");
    let editor_top_120 = rows_120
        .iter()
        .position(|r| r.contains(&"─".repeat(100)))
        .ok_or_else(|| format!("pinned editor border missing at 120x24:\n{plain_120}"))?;
    assert!(
        editor_top_120 >= 18 && rows_120.iter().any(|r| r.contains("Split Geometry")),
        "pinned editor/footer missing at 120x24:\n{plain_120}"
    );
    let div_120 = (0u16..120)
        .find(|&c| {
            screen_120
                .cell(4, c)
                .is_some_and(|cell| cell.contents() == "│")
        })
        .ok_or_else(|| {
            format!("missing side-panel divider `│` on row 4 in 120x24:\n{plain_120}")
        })?;
    assert!(
        (60..=76).contains(&div_120),
        "120x24 divider col {div_120} outside expected 3:2 split range"
    );
    for row in 4..editor_top_120 {
        let cell = screen_120.cell(row as u16, div_120).ok_or("missing cell")?;
        assert_eq!(
            cell.contents(),
            "│",
            "expected side-panel divider `│` at (row={row}, col={div_120}) in 120x24:\n{plain_120}"
        );
    }

    // Resize to 80x24: side panel hides cleanly.
    tmux.resize(80, 24)?;
    thread::sleep(Duration::from_millis(400));
    let plain_80 = tmux.capture(false)?;
    let rows_80 = assert_frame_bounds(&plain_80, 24, 80);
    assert!(
        !plain_80.contains("Built-in Commands"),
        "side panel must hide below 100 columns:\n{plain_80}"
    );
    assert!(
        rows_80.iter().any(|r| r.contains(&"─".repeat(80)))
            && rows_80.iter().any(|r| r.contains("Split Geometry")),
        "pinned editor/footer missing at 80x24:\n{plain_80}"
    );

    // Resize to 100x24: side panel reappears with continuous `│` divider and >=40-col panel.
    tmux.resize(100, 24)?;
    tmux.wait_for("Built-in Commands", Duration::from_secs(10))?;
    thread::sleep(Duration::from_millis(250));
    let ansi_100 = tmux.capture(true)?;
    let plain_100 = tmux.capture(false)?;
    let rows_100 = assert_frame_bounds(&plain_100, 24, 100);
    let editor_top_100 = rows_100
        .iter()
        .position(|r| r.contains(&"─".repeat(100)))
        .ok_or_else(|| format!("pinned editor border missing at 100x24:\n{plain_100}"))?;
    let screen_100 = parse_vt100_rows(&ansi_100, 24, 100);
    let div_100 = (0u16..100)
        .find(|&c| {
            screen_100
                .cell(4, c)
                .is_some_and(|cell| cell.contents() == "│")
        })
        .ok_or_else(|| {
            format!("missing side-panel divider `│` on row 4 in 100x24:\n{plain_100}")
        })?;
    assert!(
        (45..=60).contains(&div_100),
        "100x24 divider col {div_100} outside expected 3:2 split range"
    );
    for row in 4..editor_top_100 {
        let cell = screen_100.cell(row as u16, div_100).ok_or("missing cell")?;
        assert_eq!(
            cell.contents(),
            "│",
            "expected side-panel divider `│` at (row={row}, col={div_100}) in 100x24:\n{plain_100}"
        );
    }

    // Resize back to 120x24: side panel restores cleanly at `div_120`.
    tmux.resize(120, 24)?;
    thread::sleep(Duration::from_millis(400));
    let ansi_restored = tmux.capture(true)?;
    let plain_restored = tmux.capture(false)?;
    assert_frame_bounds(&plain_restored, 24, 120);
    let screen_restored = parse_vt100_rows(&ansi_restored, 24, 120);
    for row in 4..editor_top_120 {
        let cell = screen_restored
            .cell(row as u16, div_120)
            .ok_or("missing cell")?;
        assert_eq!(
            cell.contents(),
            "│",
            "expected restored divider `│` at (row={row}, col={div_120}) in 120x24:\n{plain_restored}"
        );
    }

    sessions.release();
    server.close().await?;
    Ok(())
}

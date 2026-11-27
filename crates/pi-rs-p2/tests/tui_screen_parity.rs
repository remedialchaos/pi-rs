//! Self-contained native `p2 tui` whole-screen PTY visual contract and `vt100` cell-grid geometry tests.

use std::{
    error::Error,
    fs,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
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

    fn send_raw(&self, bytes: &[u8]) -> TestResult {
        let mut args = vec![
            "send-keys".to_owned(),
            "-t".to_owned(),
            self.name.clone(),
            "-H".to_owned(),
        ];
        for byte in bytes {
            args.push(format!("{byte:02x}"));
        }
        let status = Command::new("tmux").args(&args).status()?;
        if !status.success() {
            return Err(format!("tmux send-keys -H failed for {}", self.name).into());
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
async fn native_whole_screen_100x24_visual_contract() -> TestResult {
    require_tool("tmux")?;

    let dir = tempfile::tempdir()?;
    let nat_data = dir.path().join("nat-data");
    let nat_cfg = dir.path().join("nat-cfg");
    let nat_home = dir.path().join("nat-home");
    for p in [&nat_data, &nat_cfg, &nat_home] {
        fs::create_dir_all(p)?;
    }

    // Start native disposable server + session + `p2 tui` in a 100x24 tmux PTY.
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

    let prompt = "Please explain how wrapping works in this terminal layout with a long paragraph that definitely exceeds eighty and one hundred columns so we can inspect wrapping.";
    nat_tmux.send_literal(prompt)?;
    nat_tmux.send_key("Enter")?;
    nat_tmux.wait_for("Echo:", Duration::from_secs(15))?;
    thread::sleep(Duration::from_millis(300));
    let nat_plain = nat_tmux.capture(false)?;
    let nat_ansi = nat_tmux.capture(true)?;
    let nat_rows = assert_frame_bounds(&nat_plain, 24, 100);

    // Parse ANSI capture with `vt100` and verify whole-screen frame, header, footer, wrapping, and theme colors.
    let nat_screen = parse_vt100_rows(&nat_ansi, 24, 100);
    assert_eq!(nat_screen.size(), (24, 100));

    // Header row presence & alignment (logo + origin · owner + help prompt).
    assert!(
        nat_rows[1].contains("▀▀█") && nat_rows[1].contains("owner"),
        "native header row 1 missing logo/owner:\n{nat_plain}"
    );
    assert!(
        nat_rows[2].contains("█▀ █") && nat_rows[2].contains("interrupt"),
        "native header row 2 missing logo/hints:\n{nat_plain}"
    );
    assert!(
        nat_rows[3].contains("Press ctrl+o to show full startup help."),
        "native header row 3 missing startup help hint:\n{nat_plain}"
    );

    // Transcript order & wrapped long paragraph.
    let user_pos = nat_plain
        .find("Please explain how wrapping works")
        .ok_or("native missing user message")?;
    let echo_pos = nat_plain
        .find("Echo:")
        .ok_or("native missing assistant Echo")?;
    assert!(
        user_pos < echo_pos,
        "native user message should precede Echo"
    );
    assert!(
        nat_plain.contains("exceeds eighty and one hundred columns so we can inspect wrapping."),
        "native missing wrapped second line of user message:\n{nat_plain}"
    );

    // Editor borders and footer/status anchored near bottom.
    let border_rows: Vec<usize> = nat_rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.contains(&"─".repeat(80)))
        .map(|(i, _)| i)
        .collect();
    assert!(
        border_rows.len() >= 2 && *border_rows.last().unwrap_or(&0) >= 19,
        "native editor borders missing near bottom: {border_rows:?}\n{nat_plain}"
    );
    assert!(
        nat_rows.iter().any(|r| r.contains("Parity Session")),
        "native footer/status missing session title:\n{nat_plain}"
    );

    // Dark-theme RGB ANSI color presence.
    assert!(
        nat_ansi.contains("38;2;228;138;122") && nat_ansi.contains("48;2;79;142;179"),
        "native missing Pi logo RGB ANSI colors"
    );
    assert!(
        nat_ansi.contains("48;2;33;59;73") && nat_ansi.contains("38;2;222;224;225"),
        "native missing dark theme userMessageBg/userMessageText ANSI colors"
    );
    let border_row = nat_rows
        .iter()
        .position(|r| r.contains(&"─".repeat(80)))
        .ok_or("native missing editor border row")?;
    let border_fg = nat_screen
        .cell(border_row as u16, 0)
        .ok_or("missing border cell")?
        .fgcolor();
    assert!(
        matches!(border_fg, vt100::Color::Rgb(r, g, b) if r < 140 && g < 145 && b < 150),
        "native unexpected dark theme borderMuted fg color: {border_fg:?}"
    );

    sessions.release();
    server.close().await?;
    Ok(())
}

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

fn wait_for_turn_echo(tmux: &TmuxSession, marker: &str, timeout: Duration) -> TestResult<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let plain = tmux.capture(false)?;
        if plain.contains("Echo:") && plain.matches(marker).count() >= 2 {
            return Ok(plain);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for assistant echo of {marker:?} in tmux {}.\nLast screen:\n{plain}",
                tmux.name
            )
            .into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mouse_scroll_and_live_theme_switch_with_integrated_worker() -> TestResult {
    require_tool("tmux")?;

    let dir = tempfile::tempdir()?;
    let home_dir = dir.path().join("home");
    let cfg_dir = dir.path().join("cfg");
    let agent_dir = dir.path().join("agent");
    for p in [&home_dir, &cfg_dir, &agent_dir] {
        fs::create_dir_all(p)?;
    }

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
                "title": "Scroll Theme E2E",
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
        "env HOME='{}' P2_DATA_DIR='{}' P2_TUI_CONFIG_DIR='{}' PI_CODING_AGENT_DIR='{}' TERM=xterm-256color '{}' tui '{}' --theme dark",
        home_dir.display(),
        dir.path().display(),
        cfg_dir.display(),
        agent_dir.display(),
        env!("CARGO_BIN_EXE_p2"),
        url
    );
    let tmux = TmuxSession::spawn("p2-parity-scroll-theme", 100, 24, &cmd)?;
    tmux.wait_for("Scroll Theme E2E", Duration::from_secs(15))?;

    // Drive enough multi-line turns against the real integrated worker to overflow the 24-row viewport.
    for turn in 1..=6 {
        let marker = format!("SCROLL_TURN_{turn:02}");
        let prompt = format!(
            "{marker} padding words to ensure each user and assistant turn wraps across multiple terminal rows cleanly."
        );
        tmux.send_literal(&prompt)?;
        tmux.send_key("Enter")?;
        wait_for_turn_echo(&tmux, &marker, Duration::from_secs(15))?;
    }
    thread::sleep(Duration::from_millis(250));

    let at_bottom = tmux.capture(false)?;
    assert_frame_bounds(&at_bottom, 24, 100);
    assert!(
        at_bottom.contains("SCROLL_TURN_06") && !at_bottom.contains("SCROLL_TURN_01"),
        "expected overflowed transcript anchored at bottom:\n{at_bottom}"
    );
    let dark_ansi = tmux.capture(true)?;
    assert!(
        dark_ansi.contains("48;2;33;59;73"),
        "expected dark theme userMessageBg ANSI sequence"
    );

    // Send SGR mouse wheel up sequences and verify the viewport scrolls up cleanly.
    for _ in 0..24 {
        tmux.send_raw(b"\x1b[<64;20;10M")?;
        thread::sleep(Duration::from_millis(25));
    }
    let scrolled_up = tmux.wait_for("SCROLL_TURN_01", Duration::from_secs(5))?;
    let up_rows = assert_frame_bounds(&scrolled_up, 24, 100);
    assert!(
        !scrolled_up.contains("SCROLL_TURN_06"),
        "expected bottom turn to scroll off-screen after wheel up:\n{scrolled_up}"
    );
    assert!(
        up_rows[1].contains("▀▀█") && up_rows.iter().any(|r| r.contains("Scroll Theme E2E")),
        "header/footer did not remain pinned while scrolling:\n{scrolled_up}"
    );

    // Send SGR mouse wheel down sequences and verify the viewport returns cleanly to the bottom.
    for _ in 0..25 {
        tmux.send_raw(b"\x1b[<65;20;10M")?;
        thread::sleep(Duration::from_millis(25));
    }
    let scrolled_back = tmux.wait_for("SCROLL_TURN_06", Duration::from_secs(5))?;
    assert_frame_bounds(&scrolled_back, 24, 100);
    assert!(
        !scrolled_back.contains("SCROLL_TURN_01"),
        "expected top turn to scroll off-screen after wheel down:\n{scrolled_back}"
    );

    // Trigger live theme switch via `/theme light` and verify ANSI/vt100 theme state updates.
    tmux.send_literal("/theme light")?;
    tmux.send_key("Enter")?;

    let deadline = Instant::now() + Duration::from_secs(5);
    let (light_plain, light_ansi) = loop {
        let plain = tmux.capture(false)?;
        let ansi = tmux.capture(true)?;
        if !ansi.contains("48;2;33;59;73") && ansi.contains("48;2;") {
            break (plain, ansi);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for light theme ANSI colors after /theme light.\nPlain:\n{plain}\nANSI:\n{ansi}"
            )
            .into());
        }
        thread::sleep(Duration::from_millis(50));
    };
    let light_rows = assert_frame_bounds(&light_plain, 24, 100);
    let light_screen = parse_vt100_rows(&light_ansi, 24, 100);
    let border_row = light_rows
        .iter()
        .position(|r| r.contains(&"─".repeat(80)))
        .ok_or("missing editor border row after theme switch")?;
    let border_fg = light_screen
        .cell(border_row as u16, 0)
        .ok_or("missing border cell")?
        .fgcolor();
    assert!(
        matches!(border_fg, vt100::Color::Rgb(r, g, b) if r > 140 && g > 140 && b > 140),
        "expected light theme borderMuted fg color after /theme light, got {border_fg:?}"
    );
    let user_row = light_rows
        .iter()
        .position(|r| r.contains("SCROLL_TURN_06"))
        .ok_or("missing user message row after theme switch")?;
    let user_bg = light_screen
        .cell(user_row as u16, 2)
        .ok_or("missing user message cell")?
        .bgcolor();
    assert!(
        matches!(user_bg, vt100::Color::Rgb(r, g, b) if r > 200 && g > 200 && b > 200),
        "expected light theme userMessageBg color after /theme light, got {user_bg:?}"
    );
    let prefs_text = fs::read_to_string(cfg_dir.join("prefs.json"))?;
    assert!(
        prefs_text.contains("\"theme\": \"light\""),
        "expected prefs.json to persist light theme: {prefs_text}"
    );

    // Live session continues working seamlessly after scrolling and switching themes.
    tmux.send_literal("AFTER_THEME_SWITCH_OK")?;
    tmux.send_key("Enter")?;
    wait_for_turn_echo(&tmux, "AFTER_THEME_SWITCH_OK", Duration::from_secs(15))?;

    tmux.send_key("C-c")?;
    thread::sleep(Duration::from_millis(200));
    sessions.release();
    server.close().await?;
    Ok(())
}

//! A PTY exercise for the guest panel: a real terminal renders a panel published by a WASM guest.
//!
//! The parent test allocates a pseudo terminal with `script(1)`, drives the child with key
//! sequences, and checks the frames the child dumps for each phase.

use super::plugins::{PanelUpdate, Plugins};
use pi_rs_p2_tui::{
    KeybindingsConfig,
    host::TuiClient,
    theme::{Theme, ThemeRef},
};
use pi_rs_tui::{ComponentRef, ProcessTerminal, Text, Tui, TuiInputListenerResult};
use std::{
    cell::Cell,
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    rc::Rc,
    thread,
    time::{Duration, Instant},
};

const CHILD_ENV: &str = "P2_GUEST_PANEL_PTY_CHILD";
const DIR_ENV: &str = "P2_GUEST_PANEL_PTY_DIR";
const FIXTURE: &[u8] =
    include_bytes!("../../../../pi-rs-p2-tui/tests/fixtures/panel/notes.tui.wasm");
const COMMAND: &str = "command";
const DEADLINE: Duration = Duration::from_secs(30);

/// Set the pseudo terminal's window size from inside the child.
fn resize(rows: u16, columns: u16) -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new("stty")
        .arg("rows")
        .arg(rows.to_string())
        .arg("cols")
        .arg(columns.to_string())
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err("cannot resize the terminal".into());
    }
    Ok(())
}

/// Render the guest panel into the terminal and record one screen dump per phase.
async fn child(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    resize(40, 120)?;
    let mut plugins = Plugins::load(dir.join("plugins"))
        .await?
        .ok_or("missing guest")?;
    let context = ["connected", "owner", "chat", "editorEmpty"];
    let id = plugins
        .slash("notes", &context)
        .ok_or("missing slash command")?
        .to_owned();
    let shortcuts = plugins.shortcuts(&KeybindingsConfig::default());
    if !shortcuts
        .iter()
        .any(|(key, command, _)| key == "f4" && command == &id)
    {
        return Err("missing key dispatch".into());
    }
    plugins.run(&id, "revision two").await?;
    let theme = ThemeRef::new(Theme::default());
    let PanelUpdate::Show(frame) = plugins.panel(&theme)? else {
        return Err("missing published panel".into());
    };
    let text = |value: &str| ComponentRef::new(Text::new(value, 0, 0, None));
    let mut client = TuiClient::mount_panel(
        ProcessTerminal::default(),
        text("chat"),
        text("transcript"),
        text("editor"),
        true,
    );
    // The terminal rule the launcher applies to a visible side panel.
    let focus = Rc::new(Cell::new(false));
    let requested = Rc::new(Cell::new(None));
    let listener_focus = focus.clone();
    let listener_request = requested.clone();
    client.screen.base.add_input_listener(Box::new(move |data| {
        let matches = |key| pi_rs_tui::keys::matches_key_data(data, key);
        if listener_focus.get() && matches("escape") {
            listener_request.set(Some(false));
            return TuiInputListenerResult {
                consume: true,
                data: None,
            };
        }
        if matches("f2") {
            listener_request.set(Some(!listener_focus.get()));
            return TuiInputListenerResult {
                consume: true,
                data: None,
            };
        }
        TuiInputListenerResult {
            consume: false,
            data: None,
        }
    }));
    client.screen.start()?;
    client.open_side_panel(frame);
    client.screen.render_now(true)?;
    dump(dir, "mounted", &client);
    let mut seen = 0usize;
    loop {
        client.step(Duration::from_millis(20))?;
        if let Some(next) = requested.take() {
            client.focus_side_panel(next);
            focus.set(next);
            client.screen.render_now(true)?;
        }
        let commands = read_commands(dir, &mut seen);
        let mut quit = false;
        for command in commands {
            match command.as_str() {
                "narrow" => resize(40, 80)?,
                "wide" => resize(40, 120)?,
                "close" => {
                    client.close_side_panel();
                }
                "quit" => quit = true,
                _ => {}
            }
            client.step(Duration::from_millis(20))?;
            client.screen.render_now(true)?;
            dump(dir, &command, &client);
        }
        if quit {
            break;
        }
    }
    client.screen.stop(Default::default())?;
    Ok(())
}

/// Record the visible screen for one phase.
fn dump(dir: &Path, phase: &str, client: &TuiClient<pi_rs_tui::TuiAltScreen>) {
    let screen = pi_rs_tui::strip_terminal_sequences(&client.screen.screen_lines().join("\n"));
    let _ = fs::write(dir.join(format!("{phase}.txt")), screen);
}

/// Read the commands the parent appended since the last call.
fn read_commands(dir: &Path, seen: &mut usize) -> Vec<String> {
    let Ok(text) = fs::read_to_string(dir.join(COMMAND)) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= *seen {
        return Vec::new();
    }
    let fresh = lines[*seen..].to_vec();
    *seen = lines.len();
    fresh.into_iter().map(str::to_owned).collect()
}

#[tokio::test]
async fn guest_panel_pty() -> Result<(), Box<dyn std::error::Error>> {
    if env::var_os(CHILD_ENV).is_some() {
        let dir = PathBuf::from(env::var(DIR_ENV)?);
        return child(&dir).await;
    }
    parent()
}

/// Drive the child through a pseudo terminal and check every phase.
fn parent() -> Result<(), Box<dyn std::error::Error>> {
    if Command::new("script").arg("--version").output().is_err() {
        eprintln!("skip: script(1) is unavailable");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    fs::create_dir(dir.path().join("plugins"))?;
    fs::write(dir.path().join("plugins").join("notes.tui.wasm"), FIXTURE)?;
    fs::write(dir.path().join(COMMAND), "")?;
    let exe = env::current_exe()?;
    let mut child = Command::new("script")
        .arg("-qec")
        .arg(format!(
            "{} --exact hosts::tui::pty::guest_panel_pty --nocapture --test-threads=1",
            exe.display()
        ))
        .arg("/dev/null")
        .env(CHILD_ENV, "1")
        .env(DIR_ENV, dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("missing child stdin")?;
    let mut stdout = child.stdout.take().ok_or("missing child stdout")?;
    let reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let phase = |name: &str| -> Result<String, Box<dyn std::error::Error>> {
        let path = dir.path().join(format!("{name}.txt"));
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Ok(text) = fs::read_to_string(&path) {
                return Ok(text);
            }
            if Instant::now() >= deadline {
                return Err(format!("phase {name} never rendered").into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    };
    let command = |name: &str| -> Result<(), Box<dyn std::error::Error>> {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join(COMMAND))?;
        writeln!(file, "{name}")?;
        Ok(())
    };
    phase("mounted")?;
    stdin.write_all(b"\x1bOQ")?;
    stdin.flush()?;
    thread::sleep(Duration::from_millis(400));
    command("focused")?;
    let focused = phase("focused")?;
    stdin.write_all(b"\x1b[6~")?;
    stdin.flush()?;
    thread::sleep(Duration::from_millis(400));
    command("scrolled")?;
    let scrolled = phase("scrolled")?;
    stdin.write_all(b"\x1b")?;
    stdin.flush()?;
    thread::sleep(Duration::from_millis(400));
    command("unfocused")?;
    phase("unfocused")?;
    command("narrow")?;
    let narrow = phase("narrow")?;
    command("wide")?;
    let wide = phase("wide")?;
    command("close")?;
    let closed = phase("close")?;
    command("quit")?;
    let output = reader.join().map_err(|_| "child output panicked")?;
    let _ = child.wait();
    let mounted = phase("mounted")?;
    if !mounted.contains("Guest notes revision two") {
        return Err("the guest panel never reached the terminal".into());
    }
    if !mounted.contains("note row 01") {
        return Err("the guest panel body never reached the terminal".into());
    }
    if !scrolled.contains("Guest notes revision two") {
        return Err("the panel lost its title while scrolling".into());
    }
    if scrolled.contains("note row 01") {
        return Err("the panel body never scrolled".into());
    }
    if !focused.contains("Guest notes revision two") {
        return Err("the panel disappeared while focused".into());
    }
    if narrow.contains("Guest notes revision two") {
        return Err("the panel stayed visible in a narrow terminal".into());
    }
    if !wide.contains("Guest notes revision two") {
        return Err("the panel did not return in a wide terminal".into());
    }
    if closed.contains("Guest notes revision two") {
        return Err("the closed panel stayed on the screen".into());
    }
    if !output.contains("Guest notes revision two") {
        return Err("the terminal never received the guest panel".into());
    }
    Ok(())
}

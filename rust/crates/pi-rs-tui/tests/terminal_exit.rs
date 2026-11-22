#[cfg(target_os = "linux")]
#[test]
fn direct_process_exit_restores_alternate_screen_and_termios() -> std::io::Result<()> {
    use std::io::{self, IsTerminal};
    use std::process::{Command, Stdio};

    use pi_rs_tui::{ProcessTerminal, Tui, TuiAltScreen, TuiAltScreenOptions};

    const CHILD: &str = "PI_RS_TUI_EXIT_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        assert!(io::stdin().is_terminal());
        let suspended = std::rc::Rc::new(std::cell::Cell::new(false));
        let event = suspended.clone();
        let mut tui = TuiAltScreen::new(
            ProcessTerminal::new(),
            false,
            TuiAltScreenOptions::default(),
        );
        tui.base
            .set_suspend_signal_handler(Some(Box::new(move || event.set(true))))?;
        tui.start()?;
        let signal = Command::new("kill")
            .args(["-s", "TSTP", &std::process::id().to_string()])
            .status()?;
        assert!(signal.success());
        tui.step(std::time::Duration::ZERO)?;
        assert!(suspended.get());
        std::process::exit(0);
    }
    let exe = std::env::current_exe()?;
    let quoted_exe = exe.to_string_lossy().replace('\'', "'\\''");
    let command = format!(
        "before=$(stty -g) && '{quoted_exe}' --exact direct_process_exit_restores_alternate_screen_and_termios --nocapture && test \"$(stty -g)\" = \"$before\""
    );
    let output = Command::new("script")
        .args(["--quiet", "--return", "--command", &command, "/dev/null"])
        .env(CHILD, "1")
        .stdin(Stdio::null())
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout:?} {:?}", output.stderr);
    assert!(stdout.contains("\x1b[?1049h"), "{stdout:?}");
    assert!(
        stdout.contains("\x1b[?2004l\x1b[<u\x1b[?1049l\x1b[?25h"),
        "{stdout:?}"
    );
    assert!(
        stdout.contains("\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?7h"),
        "{stdout:?}"
    );
    Ok(())
}

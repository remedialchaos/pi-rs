use pi_rs_p2_tui::{
    components::{SecretInput, SelectorContext, SelectorKeys},
    host::TuiClient,
    render_context::RenderContext,
    theme::{Theme, ThemeRef},
};
use pi_rs_tui::{ComponentRef, Terminal};
use std::{
    cell::RefCell,
    io,
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

struct VirtualTerminal {
    input: Vec<u8>,
    output: Arc<Mutex<String>>,
}
impl Terminal for VirtualTerminal {
    fn width(&self) -> usize {
        40
    }
    fn height(&self) -> usize {
        12
    }
    fn is_tty(&self) -> bool {
        true
    }
    fn write(&mut self, data: &str) {
        self.output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_str(data);
    }
    fn read_available(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.input)
    }
}
struct Context(ThemeRef);
impl RenderContext for Context {
    fn theme(&self) -> &ThemeRef {
        &self.0
    }
    fn key_text(&self, id: &str) -> String {
        id.into()
    }
    fn request_render(&self) {}
}
struct Keys;
impl SelectorKeys for Keys {
    fn matches(&self, data: &str, id: &str) -> bool {
        pi_rs_tui::get_keybindings().matches(data, id)
    }
}

#[test]
fn bracketed_secret_paste_is_delivered_to_focus_and_only_masks_reach_terminal() -> io::Result<()> {
    let output = Arc::new(Mutex::new(String::new()));
    let terminal = VirtualTerminal {
        output: output.clone(),
        input: b"\x1b[200~secret\x1b[201~".to_vec(),
    };
    let ctx = SelectorContext::new(
        Box::new(Context(ThemeRef::new(Theme::default()))),
        Box::new(Keys),
    );
    let input = Rc::new(RefCell::new(SecretInput::new(ctx)));
    let mut client = TuiClient::mount(terminal, ComponentRef::from_shared(input.clone()), false);
    client.screen.base.start()?;
    client.step(Duration::ZERO)?;
    client.screen.base.begin_stop();
    client.screen.base.finish_stop()?;
    assert_eq!(input.borrow().length(), 6);
    let frame = output
        .lock()
        .map_err(|_| io::Error::other("terminal capture poisoned"))?;
    assert!(frame.contains("••••••"));
    assert!(!frame.contains("secret"));
    Ok(())
}

#[test]
fn failed_shell_shows_guest_and_retained_truncation_after_completion() {
    use pi_rs_p2_tui::components::{
        BashCompleteOptions, BashExecutionComponent, BashExecutionOptions,
    };
    use pi_rs_tui::Component;
    let mut bash = BashExecutionComponent::new(
        "build".into(),
        Box::new(Context(ThemeRef::new(Theme::default()))),
        BashExecutionOptions {
            by: Some("Lin (guest)".into()),
            ..Default::default()
        },
    );
    bash.set_output("compiler output".into(), true);
    bash.set_complete(
        Some(2),
        false,
        BashCompleteOptions {
            full_output_path: Some("log.txt".into()),
            ..Default::default()
        },
    );
    let output = pi_rs_tui::strip_terminal_sequences(&bash.render(80).join("\n"));
    for part in [
        "$ build · Lin (guest)",
        "compiler output",
        "(exit 2)",
        "Output truncated. Full output: log.txt",
    ] {
        assert!(output.contains(part), "{output}");
    }
}

#[test]
fn selecting_a_row_does_not_run_its_shortcut_action() -> Result<(), Box<dyn std::error::Error>> {
    use pi_rs_p2_tui::components::{ListAction, ListOverlay, ListRow};
    use pi_rs_tui::Component;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let shortcuts = calls.clone();
    let selections = calls.clone();
    let ctx = SelectorContext::new(
        Box::new(Context(ThemeRef::new(Theme::default()))),
        Box::new(Keys),
    );
    let mut view = ListOverlay::new(
        ctx,
        vec![ListRow {
            key: "one".into(),
            label: "First".into(),
            detail: None,
            description: None,
            color: None,
        }],
        vec![ListAction {
            id: None,
            key: Some("x".into()),
            label: "archive".into(),
            run: Box::new(move |row| {
                if let Ok(mut log) = shortcuts.lock() {
                    log.push(format!("archive {}", row.map_or("", |row| &row.key)));
                }
            }),
        }],
        || {},
    )
    .with_on_select(move |row| {
        if let Ok(mut log) = selections.lock() {
            log.push(format!("open {}", row.key));
        }
    });
    view.handle_input("\r");
    view.handle_input("x");
    assert_eq!(
        *calls.lock().map_err(|_| "calls poisoned")?,
        ["open one", "archive one"]
    );
    Ok(())
}
#[test]
fn settings_preserve_mermaid_enum_and_emit_nested_path() -> Result<(), Box<dyn std::error::Error>> {
    use pi_rs_tui::Component;
    let changes = Rc::new(RefCell::new(Vec::new()));
    let changed = changes.clone();
    let prefs = pi_rs_p2_tui::preferences::sanitize_settings(
        &serde_json::json!({"markdown":{"mermaid":"final"}}),
    );
    let mut list = pi_rs_p2_tui::settings::open_settings(
        Box::new(Context(ThemeRef::new(Theme::default()))),
        &serde_json::to_value(prefs.settings)?,
        Default::default(),
        move |path, value| changed.borrow_mut().push((path.to_vec(), value.clone())),
        || {},
    );
    list.select_item("mermaid-rendering");
    list.handle_input("\r");
    assert_eq!(
        *changes.borrow(),
        [(
            vec!["markdown".to_string(), "mermaid".into()],
            serde_json::json!("streaming")
        )]
    );
    Ok(())
}

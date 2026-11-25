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

fn mount(root: ComponentRef, input: &[u8]) -> io::Result<(TuiClient, Arc<Mutex<String>>)> {
    let output = Arc::new(Mutex::new(String::new()));
    let terminal = VirtualTerminal {
        output: output.clone(),
        input: input.into(),
    };
    let mut client = TuiClient::mount(terminal, root, false);
    client.screen.base.start()?;
    Ok((client, output))
}

#[test]
fn panel_page_down_scrolls_body_and_keeps_title() -> io::Result<()> {
    use pi_rs_p2_tui::components::{PanelContent, PanelFrame};
    use pi_rs_tui::Component;
    struct Lines;
    impl Component for Lines {
        fn render(&self, _: usize) -> Vec<String> {
            (0..12).map(|i| format!("item-{i:02}")).collect()
        }
    }
    impl PanelContent for Lines {}
    let mut panel = PanelFrame::new(
        Box::new(Context(ThemeRef::new(Theme::default()))),
        "list".into(),
        Box::new(Lines),
        || "Items".into(),
    );
    panel.set_height(4);
    let (mut client, output) = mount(ComponentRef::new(panel), b"\x1b[6~")?;
    client.step(Duration::ZERO)?;
    let frame = output
        .lock()
        .map_err(|_| io::Error::other("capture poisoned"))?;
    assert!(
        frame.contains("Items") && frame.contains("item-03") && frame.contains("item-05"),
        "{frame:?}"
    );
    assert!(!frame.contains("item-06"));
    Ok(())
}

#[test]
fn tree_shows_active_branch_first_and_keeps_linear_ancestors_flat() -> io::Result<()> {
    use pi_rs_p2_tui::components::{TreeRole, TreeSelectorComponent, TreeSelectorNode};
    let nodes = [
        (1, None, "root"),
        (2, Some(1), "chain"),
        (3, Some(2), "other"),
        (4, Some(2), "active"),
    ]
    .map(|(entry, parent, text)| TreeSelectorNode {
        entry,
        parent,
        conversation: entry,
        role: TreeRole::User,
        text: text.into(),
    })
    .to_vec();
    let ctx = SelectorContext::new(
        Box::new(Context(ThemeRef::new(Theme::default()))),
        Box::new(Keys),
    );
    let tree = TreeSelectorComponent::new(ctx, nodes, |_| {}, |_| {}, || {}, Some(4));
    let (mut client, output) = mount(ComponentRef::new(tree), b"")?;
    client.step(Duration::ZERO)?;
    let frame = pi_rs_tui::strip_terminal_sequences(
        output
            .lock()
            .map_err(|_| io::Error::other("capture poisoned"))?
            .as_str(),
    );
    assert!(
        frame.contains("  • user: chain")
            && frame.contains("› ├─ • user: active")
            && frame.contains("  └─ user: other"),
        "{frame:?}"
    );
    Ok(())
}

#[test]
fn registered_plugin_receives_terminal_shortcut_before_editor() -> io::Result<()> {
    use pi_rs_p2_tui::{
        CoreAction,
        host::{TuiContribution, TuiHost},
    };
    struct Host(Vec<CoreAction>);
    impl TuiHost for Host {
        fn run_core_action(&mut self, action: CoreAction) {
            self.0.push(action);
        }
    }
    struct Plugin;
    impl TuiContribution for Plugin {
        fn id(&self) -> &str {
            "example.open"
        }
        fn run(&self, host: &mut dyn TuiHost) {
            host.run_core_action(CoreAction::PanelFocus);
        }
    }
    let ctx = SelectorContext::new(
        Box::new(Context(ThemeRef::new(Theme::default()))),
        Box::new(Keys),
    );
    let editor = Rc::new(RefCell::new(SecretInput::new(ctx)));
    let (mut client, _) = mount(ComponentRef::from_shared(editor.clone()), b"x")?;
    let host = Rc::new(RefCell::new(Host(Vec::new())));
    assert!(client.register_shortcut("x".into(), Box::new(Plugin), host.clone()));
    client.step(Duration::ZERO)?;
    assert_eq!(host.borrow().0, [CoreAction::PanelFocus]);
    assert_eq!(editor.borrow().length(), 0);
    Ok(())
}

#[test]
fn replacing_theme_file_recolors_the_mounted_component() -> Result<(), Box<dyn std::error::Error>> {
    use pi_rs_p2_tui::theme::{DARK_THEME, TerminalState};
    struct Swatch(ThemeRef);
    impl pi_rs_tui::Component for Swatch {
        fn render(&self, _: usize) -> Vec<String> {
            vec![self.0.current().fg("accent", "sample")]
        }
    }
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("theme.json");
    let mut palette = serde_json::to_value(&*DARK_THEME)?;
    palette["colors"]["accent"] = "#010203".into();
    std::fs::write(&path, serde_json::to_vec(&palette)?)?;
    let target = ThemeRef::new(Theme::default());
    let (mut client, output) = mount(ComponentRef::new(Swatch(target.clone())), b"")?;
    client.watch_theme(
        &path,
        target,
        pi_rs_tui::TerminalColorMode::Truecolor,
        TerminalState::default(),
    )?;
    client.step(Duration::ZERO)?;
    assert!(
        output
            .lock()
            .map_err(|_| "capture poisoned")?
            .contains("38;2;1;2;3m")
    );
    palette["colors"]["accent"] = "#040506".into();
    let next = dir.path().join("replacement");
    std::fs::write(&next, serde_json::to_vec(&palette)?)?;
    std::fs::rename(next, path)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        client.step(Duration::ZERO)?;
        if output
            .lock()
            .map_err(|_| "capture poisoned")?
            .contains("38;2;4;5;6m")
        {
            return Ok(());
        }
        assert!(
            std::time::Instant::now() < deadline,
            "replacement never reached the screen"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn prepending_history_retains_plugin_entry_until_its_record_changes()
-> Result<(), Box<dyn std::error::Error>> {
    use pi_rs_p2_tui::{Transcript, TranscriptComponentOptions, transcript_component};
    let calls = Rc::new(std::cell::Cell::new(0));
    let count = calls.clone();
    let mut options = TranscriptComponentOptions::default();
    options.entry_renderers.insert(
        "example".into(),
        Box::new(move |record, _, _| {
            count.set(count.get() + 1);
            Some(Box::new(pi_rs_tui::Text::new(
                record.data.as_str().unwrap_or_default(),
                0,
                0,
                None,
            )))
        }),
    );
    let item = serde_json::json!({"type":"entry","key":"7","entry":{"id":7,"kind":"example","data":"original"}});
    let initial: Transcript = serde_json::from_value(serde_json::json!({"items":[item]}))?;
    let view = Rc::new(RefCell::new(transcript_component(
        &initial, false, false, options,
    )));
    let (mut client, output) = mount(ComponentRef::from_shared(view.clone()), b"")?;
    client.step(Duration::ZERO)?;
    let mut changed =
        serde_json::json!({"items":[{"type":"failure","key":"0","text":"earlier"},item]});
    for value in ["original", "replacement"] {
        changed["items"][1]["entry"]["data"] = value.into();
        view.borrow_mut()
            .sync(&serde_json::from_value(changed.clone())?);
        client.screen.base.request_render(true);
        client.step(Duration::ZERO)?;
    }
    assert_eq!(calls.get(), 2);
    assert!(
        output
            .lock()
            .map_err(|_| "capture poisoned")?
            .contains("replacement")
    );
    Ok(())
}

#[test]
fn terminal_wheel_scrolls_panel_body_and_keeps_heading_and_editor() -> io::Result<()> {
    use pi_rs_tui::{Text, Tui};
    let text = |value: &str| ComponentRef::new(Text::new(value, 0, 0, None));
    let rows = (0..32)
        .map(|i| format!("row-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut client = TuiClient::mount_panel(
        VirtualTerminal {
            input: b"\x1b[<64;10;4M".to_vec(),
            output: Default::default(),
        },
        text("heading"),
        text(&rows),
        text("editor"),
        true,
    );
    client.screen.start()?;
    client.screen.render_now(true)?;
    let before = client.screen.viewport_top();
    assert!(client.screen.screen_lines().join("\n").contains("row-31"));
    client.step(Duration::ZERO)?;
    client.screen.render_now(false)?;
    assert!(client.screen.viewport_top() < before);
    let frame = client.screen.screen_lines();
    assert!(frame.first().is_some_and(|line| line.contains("heading")));
    assert!(frame.last().is_some_and(|line| line.contains("editor")));
    Ok(())
}

#[test]
fn mounted_side_panel_keeps_focus_and_scroll_on_update_and_releases_on_close() -> io::Result<()> {
    use pi_rs_p2_tui::components::{PanelContent, PanelFrame};
    use pi_rs_tui::{Component, Text, Tui};
    use std::cell::Cell;

    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Wide(Arc<AtomicUsize>);
    impl Terminal for Wide {
        fn width(&self) -> usize {
            self.0.load(Ordering::Relaxed)
        }
        fn height(&self) -> usize {
            10
        }
        fn write(&mut self, _: &str) {}
        fn is_tty(&self) -> bool {
            true
        }
        fn read_available(&mut self) -> Vec<u8> {
            Vec::new()
        }
    }
    struct Notes(Rc<Cell<usize>>);
    impl Component for Notes {
        fn render(&self, _: usize) -> Vec<String> {
            (1..=30).map(|i| format!("note {i:02}")).collect()
        }
    }
    impl PanelContent for Notes {
        fn dispose(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    let width = Arc::new(AtomicUsize::new(100));
    let disposed = Rc::new(Cell::new(0));
    let panel = |title: &'static str| {
        PanelFrame::new(
            Box::new(Context(ThemeRef::new(Theme::default()))),
            "notes".into(),
            Box::new(Notes(disposed.clone())),
            move || title.into(),
        )
    };
    let text = |s| ComponentRef::new(Text::new(s, 0, 0, None));
    let mut client = TuiClient::mount_panel(
        Wide(width.clone()),
        text("chat"),
        text("transcript"),
        text("editor"),
        true,
    );
    client.screen.start()?;
    client.open_side_panel(panel("first version"));
    client.screen.render_now(true)?;
    assert!(
        client
            .screen
            .screen_lines()
            .iter()
            .any(|line| line.contains("transcript") && line.contains("first version"))
    );
    assert!(client.focus_side_panel(true));
    client.screen.handle_input("\x1b[6~");
    client.open_side_panel(panel("second version"));
    client.screen.render_now(false)?;
    let frame = client.screen.screen_lines().join("\n");
    assert!(
        client.side_panel_focused()
            && frame.contains("second version")
            && !frame.contains("note 01")
    );
    assert_eq!(disposed.get(), 1);
    width.store(99, Ordering::Relaxed);
    client.step(Duration::ZERO)?;
    client.screen.render_now(true)?;
    assert!(!client.side_panel_focused());
    assert!(
        !client
            .screen
            .screen_lines()
            .join("\n")
            .contains("second version")
    );
    width.store(100, Ordering::Relaxed);
    client.screen.render_now(true)?;
    assert!(
        client
            .screen
            .screen_lines()
            .join("\n")
            .contains("second version")
    );
    assert!(client.close_side_panel());
    client.screen.render_now(true)?;
    assert_eq!(disposed.get(), 2);
    assert!(
        !client
            .screen
            .screen_lines()
            .join("\n")
            .contains("second version")
    );
    Ok(())
}

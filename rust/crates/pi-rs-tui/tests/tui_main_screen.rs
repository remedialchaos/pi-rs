use std::cell::RefCell;
use std::io;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use pi_rs_tui::tui::{
    CURSOR_MARKER, Component, ComponentRef, OverlayAnchor, OverlayOptions, SizeValue, Tui,
};
use pi_rs_tui::tui_main_screen::TuiMainScreen;

mod support;
use support::VirtualTerminal;

struct TextLines(Vec<String>);
impl Component for TextLines {
    fn render(&self, _width: usize) -> Vec<String> {
        self.0.clone()
    }
}

fn new_tui() -> (TuiMainScreen, Arc<Mutex<vt100::Parser>>) {
    let terminal = VirtualTerminal::new(4, 16);
    let parser = terminal.parser.clone();
    (TuiMainScreen::new(terminal, true, None), parser)
}

#[test]
fn updates_above_the_cursor_and_erases_disappearing_rows() -> io::Result<()> {
    let (mut tui, parser) = new_tui();
    let lines = Rc::new(RefCell::new(TextLines(vec![
        "heading".into(),
        format!("$ hi{CURSOR_MARKER}"),
        "status".into(),
    ])));
    tui.base
        .content
        .add_child(ComponentRef::from_shared(lines.clone()));
    tui.render_now(false)?;
    {
        let parser = parser
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(parser.screen().contents(), "heading\n$ hi\nstatus");
        assert_eq!(parser.screen().cursor_position(), (1, 4));
    }
    {
        let mut lines = lines.borrow_mut();
        lines.0[0] = "new".into();
        lines.0.pop();
    }
    tui.render_now(false)?;
    let parser = parser
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(parser.screen().contents(), "new\n$ hi");
    assert_eq!(parser.screen().cursor_position(), (1, 4));
    Ok(())
}

#[test]
fn closing_an_overlay_restores_a_split_wide_character() -> io::Result<()> {
    let (mut tui, parser) = new_tui();
    tui.base
        .content
        .add_child(ComponentRef::new(TextLines(vec!["字123".into()])));
    tui.render_now(false)?;
    let overlay = tui.base.show_overlay(
        ComponentRef::new(TextLines(vec!["x".into()])),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Cells(1)),
            col: Some(SizeValue::Cells(1)),
            ..Default::default()
        },
    );
    tui.render_now(false)?;
    {
        let parser = parser
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(parser.screen().contents().starts_with(" x123"));
    }
    tui.base.hide_overlay(overlay);
    tui.render_now(false)?;
    let parser = parser
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(parser.screen().contents(), "字123");
    Ok(())
}

#[test]
fn differential_frames_match_upstream_xterm_viewports() -> io::Result<()> {
    use serde_json::Value;
    for fixture in include_str!("fixtures/main.jsonl").lines() {
        let trace: Value = serde_json::from_str(fixture).map_err(io::Error::other)?;
        let missing = || io::Error::other("malformed main-screen fixture");
        let dimension = |value: &Value| {
            value
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .ok_or_else(missing)
        };
        let rows = dimension(&trace["rows"])?;
        let cols = dimension(&trace["cols"])?;
        let terminal = VirtualTerminal::new(rows, cols);
        let parser = terminal.parser.clone();
        let mut tui = TuiMainScreen::new(terminal, true, None);
        let lines = Rc::new(RefCell::new(TextLines(Vec::new())));
        tui.base
            .content
            .add_child(ComponentRef::from_shared(lines.clone()));
        tui.start()?;
        tui.render_now(false)?;
        for (i, frame) in trace["frames"]
            .as_array()
            .ok_or_else(missing)?
            .iter()
            .enumerate()
        {
            lines.borrow_mut().0 = frame["lines"]
                .as_array()
                .ok_or_else(missing)?
                .iter()
                .map(|line| line.as_str().map(str::to_owned).ok_or_else(missing))
                .collect::<io::Result<_>>()?;
            tui.render_now(false)?;
            let terminal = parser
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let screen = terminal.screen();
            let actual = screen.rows(0, cols).collect::<Vec<_>>();
            let expected = frame["visible"]
                .as_array()
                .ok_or_else(missing)?
                .iter()
                .map(|line| line.as_str().ok_or_else(missing))
                .collect::<io::Result<Vec<_>>>()?;
            assert_eq!(actual, expected, "trace {:?}, frame {i}", trace["name"]);
            if let Some(cursor) = frame.get("cursor") {
                assert_eq!(
                    screen.cursor_position(),
                    (dimension(&cursor["y"])?, dimension(&cursor["x"])?)
                );
            }
        }
    }
    Ok(())
}

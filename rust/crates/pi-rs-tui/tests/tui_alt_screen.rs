use std::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;

use pi_rs_tui::alt_screen_search::{AltScreenSearchSegment, find_alt_screen_search_matches};
use pi_rs_tui::{
    Component, ComponentRef, Focusable, Input, ScrollView, ScrollViewOptions, Scrollbar, Tui,
    TuiAltScreen, TuiAltScreenOptions, TuiStopOptions, ViewportTui,
};

mod support;
use support::VirtualTerminal;

struct Transcript {
    lines: Vec<String>,
    renders: Rc<Cell<usize>>,
}
impl Component for Transcript {
    fn render(&self, _width: usize) -> Vec<String> {
        self.renders.set(self.renders.get() + 1);
        self.lines.clone()
    }
}

fn transcript() -> (ComponentRef, Rc<Cell<usize>>) {
    let renders = Rc::new(Cell::new(0));
    let lines = (0..30)
        .map(|row| {
            if row % 10 == 0 {
                format!("\x1b]133;A\x07prompt {row}")
            } else if row % 8 == 1 {
                format!("needle {row}")
            } else {
                format!("row {row}")
            }
        })
        .collect();
    (
        ComponentRef::new(Transcript {
            lines,
            renders: renders.clone(),
        }),
        renders,
    )
}

#[test]
fn wheel_frames_reuse_output_and_prompt_jumps_strip_shell_markers() -> io::Result<()> {
    let terminal = VirtualTerminal::new(6, 48);
    let mut tui = TuiAltScreen::new(terminal.clone(), false, TuiAltScreenOptions::default());
    let (doc, renders) = transcript();
    tui.base.content.add_child(doc);
    tui.start()?;
    tui.render_now(false)?;
    assert_eq!(tui.viewport_top(), 24);
    let count = renders.get();
    tui.handle_input("\x1b[<64;3;3M");
    tui.render_now(false)?;
    assert_eq!(tui.viewport_top(), 23);
    assert_eq!(renders.get(), count);
    tui.handle_input("\x1b[1;6A");
    tui.render_now(false)?;
    assert_eq!(tui.viewport_top(), 20);
    assert!(!tui.screen_lines()[0].contains("\x1b]133;"));
    {
        let parser = terminal
            .parser
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(parser.screen().alternate_screen());
        assert!(parser.screen().contents().starts_with("prompt 20\nrow 21"));
    }
    tui.stop(TuiStopOptions {
        preserve_screen: true,
    })?;
    assert!(
        !terminal
            .parser
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .screen()
            .alternate_screen()
    );
    Ok(())
}

#[test]
fn search_scrolls_to_the_match_and_restores_input_focus() -> io::Result<()> {
    let terminal = VirtualTerminal::new(8, 66);
    let mut tui = TuiAltScreen::new(terminal, false, TuiAltScreenOptions::default());
    let (doc, _) = transcript();
    let input = Rc::new(RefCell::new(Input::default()));
    let editor = ComponentRef::from_shared(input.clone());
    tui.base.content.add_child(doc);
    tui.base.content.add_child(editor.clone());
    tui.base.set_focus(Some(editor));
    tui.start()?;
    tui.render_now(false)?;
    tui.handle_input("\x1b[102;6u");
    tui.handle_input("needle");
    tui.render_now(false)?;
    assert!(tui.screen_lines().iter().any(|line| line.contains("4/4")));
    tui.handle_input("\r");
    tui.render_now(false)?;
    assert_eq!(tui.viewport_top(), 0);
    assert!(
        tui.screen_lines()
            .iter()
            .any(|line| line.contains("\x1b[1;7mneedle"))
    );
    tui.handle_input("\x1b");
    tui.render_now(false)?;
    assert!(input.borrow().is_focused());
    assert_eq!(input.borrow().value(), "");
    Ok(())
}

#[test]
fn search_maps_unicode_and_ascii_runs_to_cells_across_rows() {
    let lines = [
        "\x1b[31mfoo  bar\x1b[0m".to_owned(),
        "A界🙂e\u{301}Z".to_owned(),
    ];
    let matches = find_alt_screen_search_matches(&lines, "oo   bar\nA界🙂e\u{301}");
    assert_eq!(
        matches
            .into_iter()
            .map(|hit| hit.segments)
            .collect::<Vec<_>>(),
        [vec![
            AltScreenSearchSegment {
                row: 0,
                start_col: 1,
                end_col: 3
            },
            AltScreenSearchSegment {
                row: 0,
                start_col: 5,
                end_col: 8
            },
            AltScreenSearchSegment {
                row: 1,
                start_col: 0,
                end_col: 6
            },
        ]]
    );
}

#[test]
fn scrollbar_drag_reaches_end_and_selection_copies_whole_wide_characters() -> io::Result<()> {
    let terminal = VirtualTerminal::new(6, 32);
    let copied = Rc::new(RefCell::new(String::new()));
    let clipboard = copied.clone();
    let options = TuiAltScreenOptions {
        copy_selection: Some(Box::new(move |text| {
            clipboard.borrow_mut().push_str(text);
            Ok(())
        })),
        ..Default::default()
    };
    let mut tui = TuiAltScreen::new(terminal, false, options);
    let lines: Vec<_> = (0..20).map(|i| format!("a界 row {i}")).collect();
    let document = ComponentRef::new(Transcript {
        lines,
        renders: Rc::new(Cell::new(0)),
    });
    let scroll = ScrollView::new(
        document,
        ScrollViewOptions {
            scrollbar: Scrollbar::Always,
            ..Default::default()
        },
    );
    tui.set_layout_root(Some(ComponentRef::new(scroll)));
    tui.start()?;
    tui.render_now(false)?;
    tui.handle_input("\x1b[<0;32;1M");
    tui.handle_input("\x1b[<32;32;6M");
    tui.handle_input("\x1b[<0;32;6m");
    tui.render_now(false)?;
    assert_eq!(tui.viewport_top(), 14);
    tui.handle_input("\x1b[<0;3;2M");
    tui.handle_input("\x1b[<32;5;2M");
    tui.handle_input("\x1b[<0;5;2m");
    assert_eq!(*copied.borrow(), "界 r");
    Ok(())
}

#[test]
fn clicking_an_input_inside_a_padded_box_focuses_the_child() -> io::Result<()> {
    let terminal = VirtualTerminal::new(5, 24);
    let mut tui = TuiAltScreen::new(terminal, false, TuiAltScreenOptions::default());
    let input = Rc::new(RefCell::new(Input::default()));
    let mut panel = pi_rs_tui::Box::new(2, 1, None);
    panel.add_child(ComponentRef::from_shared(input.clone()));
    tui.base.content.add_child(ComponentRef::new(panel));
    tui.start()?;
    tui.render_now(false)?;
    tui.handle_input("\x1b[<0;5;2M");
    tui.handle_input("\x1b[<0;5;2m");
    tui.handle_input("a");
    assert_eq!(input.borrow().value(), "a");
    assert!(input.borrow().is_focused());
    Ok(())
}

use std::cell::RefCell;
use std::io;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use pi_rs_tui::autocomplete::{AppliedCompletion, AutocompleteOptions};
use pi_rs_tui::components::editor::word_wrap_line;
use pi_rs_tui::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, Component, Editor,
    EditorAutocomplete, EditorComponent, EditorHistory, EditorOptions, EditorTheme, Input,
    strip_terminal_sequences,
};

fn editor() -> Editor {
    Editor::new(
        Box::new(|| 24),
        Rc::new(|| {}),
        EditorTheme::default(),
        EditorOptions::default(),
    )
}

#[test]
fn cjk_wrap_keeps_byte_ranges_and_preserves_text() {
    let source = "abc你好世界xyz";
    let chunks = word_wrap_line(source, 5);
    assert_eq!(
        chunks.iter().map(|c| c.text).collect::<Vec<_>>(),
        ["abc你", "好世", "界xyz"]
    );
    for chunk in chunks {
        assert_eq!(chunk.text, &source[chunk.start_index..chunk.end_index]);
    }
}

#[test]
fn multiline_paste_is_atomic_and_undo_restores_its_registration() {
    let mut editor = editor();
    let paste = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven";
    editor.handle_input("\x1b[200~one\x1b[106;5utwo\nthree\nfour\nfive\n");
    editor.handle_input("six\nseven\neight\nnine\nten\neleven\x1b[201~");
    assert_eq!(editor.text(), "[paste #1 +11 lines]");
    assert_eq!(editor.expanded_text(), paste);
    editor.handle_input("\x7f");
    assert_eq!(editor.text(), "");
    editor.handle_input("\x1b[45;5u");
    assert_eq!(editor.expanded_text(), paste);
    let submitted = Rc::new(RefCell::new(String::new()));
    let output = submitted.clone();
    editor.set_on_submit(Some(Box::new(move |text| {
        output.borrow_mut().push_str(text)
    })));
    editor.handle_input("\r");
    assert_eq!(&*submitted.borrow(), paste);
}

#[test]
fn history_preserves_draft_cursor_and_grapheme_deletion_is_atomic() {
    let mut editor = editor();
    editor.add_to_history("old");
    editor.set_text("a👩‍🚀b");
    editor.handle_input("\x1b[D");
    editor.handle_input("\x7f");
    assert_eq!(editor.text(), "ab");
    editor.handle_input("\x1b[45;5u");
    assert_eq!(editor.text(), "a👩‍🚀b");
    editor.handle_input("\x01");
    editor.handle_input("\x1b[A");
    assert_eq!(editor.text(), "old");
    editor.handle_input("\x1b[B");
    assert_eq!(editor.text(), "a👩‍🚀b");
    assert_eq!(editor.cursor(), (0, 0));
}

#[test]
fn single_line_paste_normalizes_input_as_one_undo_step() {
    let mut input = Input::default();
    input.handle_input("字");
    input.handle_input("\x1b[200~abc\r\n\t👩‍🚀\x1b[201~");
    assert_eq!(input.value(), "字abc    👩‍🚀");
    input.handle_input("\x1b[45;5u");
    assert_eq!(input.value(), "字");
    assert_eq!(strip_terminal_sequences(&input.render(4)[0]), ">   ");
}

struct Provider(mpsc::SyncSender<String>);
impl AutocompleteProvider for Provider {
    fn get_suggestions(
        &self,
        lines: &[String],
        row: usize,
        col: usize,
        options: AutocompleteOptions<'_>,
    ) -> Option<AutocompleteSuggestions> {
        let prefix = &lines.get(row)?[..col];
        self.0.send(prefix.to_owned()).ok()?;
        (!options.aborted()).then(|| AutocompleteSuggestions {
            prefix: prefix.to_owned(),
            items: vec![AutocompleteItem {
                value: "file".into(),
                label: "file".into(),
                description: None,
            }],
        })
    }
    fn apply_completion(
        &self,
        mut lines: Vec<String>,
        row: usize,
        col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> AppliedCompletion {
        lines[row].replace_range(col - prefix.len()..col, &item.value);
        AppliedCompletion {
            lines,
            cursor_line: row,
            cursor_col: col - prefix.len() + item.value.len(),
        }
    }
}

#[test]
fn explicit_file_completion_inserts_the_only_match_and_is_undoable() -> io::Result<()> {
    let mut editor = editor();
    let (s, r) = mpsc::sync_channel(1);
    editor.set_autocomplete_provider(Box::new(Provider(s)));
    editor.set_text("fi");
    let changes = Rc::new(RefCell::new(Vec::new()));
    let observed = changes.clone();
    editor.set_on_change(Some(Box::new(move |text| {
        observed.borrow_mut().push(text.to_owned())
    })));
    editor.handle_input("\t");
    let seen = r
        .recv_timeout(Duration::from_secs(2))
        .map_err(io::Error::other)?;
    assert_eq!(seen, "fi");
    let deadline = Instant::now() + Duration::from_secs(2);
    while editor.text() == "fi" && Instant::now() < deadline {
        editor.tick(Instant::now());
        std::thread::yield_now();
    }
    assert_eq!(editor.text(), "file");
    assert_eq!(*changes.borrow(), ["file"]);
    editor.handle_input("\x1b[45;5u");
    assert_eq!(editor.text(), "fi");
    Ok(())
}

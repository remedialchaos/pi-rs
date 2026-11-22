use std::{cell::Cell, io, rc::Rc};

use pi_rs_tui::{
    Component, Editor, EditorAppearance, EditorComponent, EditorHistory, EditorInsertion,
    EditorOptions, EditorTheme, Focusable, Input, InputOptions, strip_terminal_sequences,
};
use serde_json::Value;

fn malformed() -> io::Error {
    io::Error::other("malformed editor oracle fixture")
}
fn number(item: &Value) -> io::Result<usize> {
    item.as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(malformed)
}
fn text(item: &Value) -> io::Result<&str> {
    item.as_str().ok_or_else(malformed)
}
fn rendered(lines: Vec<String>) -> Vec<String> {
    lines
        .iter()
        .map(|line| strip_terminal_sequences(line).trim_end().to_owned())
        .collect()
}
fn expected_render(item: &Value) -> io::Result<Vec<String>> {
    item.as_array()
        .ok_or_else(malformed)?
        .iter()
        .map(|line| text(line).map(str::to_owned))
        .collect()
}
fn short(value: &str) -> &str {
    &value[..value.floor_char_boundary(value.len().min(180))]
}

#[test]
fn upstream_input_operations_keep_value_cursor_and_rendered_rows() -> io::Result<()> {
    let mut failures = Vec::new();
    for (index, case) in include_str!("fixtures/input.jsonl").lines().enumerate() {
        let case: Value = serde_json::from_str(case).map_err(io::Error::other)?;
        let options = &case["options"];
        let style =
            if let [open, close] = options["style"].as_array().map_or(&[][..], Vec::as_slice) {
                let (open, close) = (text(open)?.to_owned(), text(close)?.to_owned());
                Some(Rc::new(move |s: &str| format!("{open}{s}{close}"))
                    as pi_rs_tui::components::TextStyleFn)
            } else {
                None
            };
        let mut input = Input::new(InputOptions {
            prompt: options["prompt"].as_str().map(str::to_owned),
            placeholder: options["placeholder"].as_str().unwrap_or("").to_owned(),
            placeholder_style: style,
        });
        let mut value = "";
        for (step, op) in case["operations"]
            .as_array()
            .ok_or_else(malformed)?
            .iter()
            .enumerate()
        {
            let arg = &op["args"][0];
            let state = &op["state"];
            value = state[0].as_str().unwrap_or(value);
            input.set_focused(op["focused"].as_bool().ok_or_else(malformed)?);
            let mut actual = None;
            match text(&op["method"])? {
                "handleInput" => input.handle_input(text(arg)?),
                "setValue" => input.set_value(text(arg)?.to_owned()),
                "render" => actual = Some(rendered(input.render(number(arg)?))),
                _ => return Err(malformed()),
            }
            let cursor = input.value()[..input.cursor()].encode_utf16().count();
            let expected = number(&state[1])?;
            if input.value() != value || cursor != expected {
                failures.push(format!(
                    "case {index} step {step} {} {arg} expected {:?}@{expected}, got {:?}@{cursor}",
                    op["method"],
                    short(value),
                    short(input.value())
                ));
                break;
            }
            if let Some(actual) = actual {
                let expected = expected_render(&op["render"])?;
                if actual != expected {
                    failures.push(format!("case {index} step {step} render {arg} expected {expected:?}, got {actual:?}"));
                    break;
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} input traces diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
    Ok(())
}

#[test]
fn upstream_editor_operations_keep_value_cursor_and_rendered_rows() -> io::Result<()> {
    let mut failures = Vec::new();
    for (index, case) in include_str!("fixtures/editor.jsonl").lines().enumerate() {
        let case: Value = serde_json::from_str(case).map_err(io::Error::other)?;
        let options = &case["options"];
        let rows = Rc::new(Cell::new(24_usize));
        let rows_in_editor = rows.clone();
        let mut editor = Editor::new(
            Box::new(move || rows_in_editor.get()),
            Rc::new(|| {}),
            EditorTheme::default(),
            EditorOptions {
                padding_x: number(&options["paddingX"]).unwrap_or(0),
                autocomplete_max_visible: number(&options["autocompleteMaxVisible"]).unwrap_or(5),
            },
        );
        let mut value = "";
        for (step, op) in case["operations"]
            .as_array()
            .ok_or_else(malformed)?
            .iter()
            .enumerate()
        {
            let arg = &op["args"][0];
            let state = &op["state"];
            value = state[0].as_str().unwrap_or(value);
            rows.set(number(&op["rows"])?);
            editor.disable_submit = op["disabled"].as_bool().ok_or_else(malformed)?;
            editor.set_focused(op["focused"].as_bool().ok_or_else(malformed)?);
            let mut actual = None;
            match text(&op["method"])? {
                "handleInput" => editor.handle_input(text(arg)?),
                "setText" => editor.set_text(text(arg)?),
                "insertTextAtCursor" => editor.insert_text_at_cursor(text(arg)?),
                "addToHistory" => editor.add_to_history(text(arg)?),
                "setPaddingX" => editor.set_padding_x(number(arg)?),
                "render" => actual = Some(rendered(editor.render(number(arg)?))),
                _ => return Err(malformed()),
            }
            let (row, byte) = editor.cursor();
            let line = editor.lines().nth(row).ok_or_else(malformed)?;
            let col = line[..byte].encode_utf16().count();
            let (a, b) = (number(&state[1])?, number(&state[2])?);
            if editor.text() != value || row != a || col != b {
                failures.push(format!(
                    "case {index} step {step} {} {} expected {:?}@{a}:{b}, got {:?}@{row}:{col}",
                    op["method"],
                    short(&arg.to_string()),
                    short(value),
                    short(editor.text())
                ));
                break;
            }
            if let Some(actual) = actual {
                let expected = expected_render(&op["render"])?;
                if actual != expected {
                    failures.push(format!("case {index} step {step} render {arg} expected {expected:?}, got {actual:?}"));
                    break;
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} editor traces diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
    Ok(())
}

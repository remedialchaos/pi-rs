use std::borrow::Cow;
use std::io;

use pi_rs_tui::utils::{
    extract_segments, get_grapheme_cell_range, normalize_terminal_output, slice_by_column,
    slice_with_width, truncate_with_ellipsis, visible_width, wrap_text_with_ansi,
};
use serde_json::{Value, json};

#[test]
fn upstream_terminal_utilities_preserve_cells_and_escape_order() -> io::Result<()> {
    let mut differences = Vec::new();
    for (i, line) in include_str!("fixtures/utils.jsonl").lines().enumerate() {
        let case: Value = serde_json::from_str(line).map_err(io::Error::other)?;
        let missing = || io::Error::other(format!("invalid utility fixture at line {}", i + 1));
        let method = case["method"].as_str().ok_or_else(missing)?;
        let args = &case["args"];
        let integer = |value: &Value| {
            value
                .as_u64()
                .and_then(|v| usize::try_from(v).ok())
                .ok_or_else(missing)
        };
        let n = |i: usize| integer(&args[i]);
        let b = |i: usize| args[i].as_bool().unwrap_or(false);
        let text = if let Some(s) = args[0].as_str() {
            Cow::Borrowed(s)
        } else {
            let a = &args[0];
            let part = |i: usize| a[i].as_str().ok_or_else(missing);
            Cow::Owned(format!(
                "{}{}{}",
                part(0)?,
                part(1)?.repeat(integer(&a[2])?),
                part(3)?
            ))
        };
        let actual = match method {
            "visibleWidth" => json!(visible_width(&text)),
            "normalizeTerminalOutput" => json!(normalize_terminal_output(&text)),
            "wrapTextWithAnsi" => json!(wrap_text_with_ansi(&text, n(1)?)),
            "truncateToWidth" => json!(truncate_with_ellipsis(
                &text,
                n(1)?,
                args[2].as_str().unwrap_or("..."),
                b(3)
            )),
            "getGraphemeCellRange" => get_grapheme_cell_range(&text, n(1)?)
                .map_or(Value::Null, |r| json!({"start": r.start, "end": r.end})),
            "sliceByColumn" => json!(slice_by_column(&text, n(1)?, n(2)?, b(3))),
            "sliceWithWidth" => {
                let s = slice_with_width(&text, n(1)?, n(2)?, b(3));
                json!({"text":s.text,"width":s.width})
            }
            "extractSegments" => {
                let s = extract_segments(&text, n(1)?, n(2)?, n(3)?, b(4));
                json!({"before":s.before,"beforeWidth":s.before_width,"after":s.after,"afterWidth":s.after_width})
            }
            _ => return Err(missing()),
        };
        if actual != case["result"] {
            differences.push(format!(
                "line {} {method}: expected {:?}, got {actual:?}",
                i + 1,
                case["result"]
            ));
        }
    }
    assert!(
        differences.is_empty(),
        "{} utility differences:\n{}",
        differences.len(),
        differences.join("\n")
    );
    Ok(())
}

//! A turn's activity: steps, wheels and the text helpers (upstream `activity/model.ts`).

use std::collections::HashMap;

/// The status of one tool call step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepStatus {
    Running,
    Done,
    Error,
}

/// One tool call of an activity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub id: String,
    pub name: String,
    pub status: StepStatus,
    pub output: String,
    pub duration_ms: Option<u64>,
}

impl Step {
    pub fn new(id: impl Into<String>, name: impl Into<String>, status: StepStatus) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            status,
            output: String::new(),
            duration_ms: None,
        }
    }
}

/// How much of a turn the transcript shows (settings.json `detail`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Detail {
    One = 1,
    #[default]
    Two = 2,
    Three = 3,
    Four = 4,
}

/// A thinking block or text shown on a wheel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Words {
    pub kind: WordsKind,
    pub id: String,
    pub text: String,
    pub live: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordsKind {
    Thought,
    Text,
}

/// What a wheel rolls through, in order: calls and, at details 1 and 2, thinking/texts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WheelEntry {
    Call(Step),
    Words(Words),
}

/// A tool's words for what it does, contributed by plugins by tool name.
/// Function-pointer fields are deliberately not compared for equality (their addresses
/// are not guaranteed unique), so this type is `Clone + Debug + Default` only.
#[derive(Clone, Debug, Default)]
pub struct ToolActivity {
    /// One or two words while it runs.
    pub brief: Option<String>,
    /// A failure's reason ("exit 1"); without it, "failed".
    pub failure: Option<String>,
    /// Words for a finished run, counted ("ran a command" / "ran 3 commands").
    pub summary: Option<fn(usize) -> String>,
    /// How a group of repeated identical calls says so ("20 runs, 1 failed").
    pub repeated: Option<fn(usize, usize) -> String>,
    /// Whether a call made a result for the person, and where.
    pub result_placement: Option<ResultPlacement>,
}

/// Where a call's result for the person shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultPlacement {
    After,
    Inline,
}

/// Contributions by tool name.
pub type ToolActivityTable = HashMap<String, ToolActivity>;

fn bash_summary(count: usize) -> String {
    if count == 1 {
        "ran a command".to_owned()
    } else {
        format!("ran {count} commands")
    }
}

fn read_summary(count: usize) -> String {
    if count == 1 {
        "read a file".to_owned()
    } else {
        format!("read {count} files")
    }
}

fn write_summary(count: usize) -> String {
    if count == 1 {
        "wrote a file".to_owned()
    } else {
        format!("wrote {count} files")
    }
}

fn edit_summary(count: usize) -> String {
    if count == 1 {
        "edited a file".to_owned()
    } else {
        format!("edited {count} files")
    }
}

fn search_summary(count: usize) -> String {
    if count == 1 {
        "searched".to_owned()
    } else {
        format!("ran {count} searches")
    }
}

fn artifact_summary(count: usize) -> String {
    if count == 1 {
        "updated an artifact".to_owned()
    } else {
        format!("updated {count} artifacts")
    }
}

fn terminal_summary(count: usize) -> String {
    if count == 1 {
        "used a terminal".to_owned()
    } else {
        format!("used terminals {count} times")
    }
}

fn subagent_summary(count: usize) -> String {
    if count == 1 {
        "ran a subagent".to_owned()
    } else {
        format!("ran {count} subagents")
    }
}

/// Built-in tool activity descriptors matching upstream `coding-tools`, `artifacts`,
/// `terminals`, and `subagents` web facets.
pub fn default_tool_activities() -> ToolActivityTable {
    let mut table = ToolActivityTable::new();
    table.insert(
        "bash".to_owned(),
        ToolActivity {
            brief: Some("Running".to_owned()),
            failure: None,
            summary: Some(bash_summary),
            repeated: None,
            result_placement: None,
        },
    );
    table.insert(
        "read".to_owned(),
        ToolActivity {
            brief: Some("Reading".to_owned()),
            failure: None,
            summary: Some(read_summary),
            repeated: None,
            result_placement: None,
        },
    );
    table.insert(
        "write".to_owned(),
        ToolActivity {
            brief: Some("Writing".to_owned()),
            failure: None,
            summary: Some(write_summary),
            repeated: None,
            result_placement: None,
        },
    );
    table.insert(
        "edit".to_owned(),
        ToolActivity {
            brief: Some("Editing".to_owned()),
            failure: None,
            summary: Some(edit_summary),
            repeated: None,
            result_placement: None,
        },
    );
    for name in ["grep", "find", "ls"] {
        table.insert(
            name.to_owned(),
            ToolActivity {
                brief: Some("Searching".to_owned()),
                failure: None,
                summary: Some(search_summary),
                repeated: None,
                result_placement: None,
            },
        );
    }
    for name in [
        "artifact_list",
        "artifact_read",
        "artifact_write",
        "artifact_edit",
        "artifact_open",
        "artifact_delete",
        "artifact_html_errors",
        "artifact_html_logs",
        "artifact_html_eval",
        "artifact_html_state",
        "artifact_html_set_state",
    ] {
        table.insert(
            name.to_owned(),
            ToolActivity {
                brief: Some("Artifact".to_owned()),
                failure: None,
                summary: Some(artifact_summary),
                repeated: None,
                result_placement: None,
            },
        );
    }
    table.insert(
        "artifact_show".to_owned(),
        ToolActivity {
            brief: Some("Showing artifact".to_owned()),
            failure: None,
            summary: Some(artifact_summary),
            repeated: None,
            result_placement: Some(ResultPlacement::Inline),
        },
    );
    table.insert(
        "terminal".to_owned(),
        ToolActivity {
            brief: Some("Terminal".to_owned()),
            failure: None,
            summary: Some(terminal_summary),
            repeated: None,
            result_placement: None,
        },
    );
    table.insert(
        "subagent".to_owned(),
        ToolActivity {
            brief: Some("Subagent".to_owned()),
            failure: None,
            summary: Some(subagent_summary),
            repeated: None,
            result_placement: None,
        },
    );
    table
}

/// The tool status of a raw status string (`toolStatus` / `callStatus`).
pub fn call_status(status: &str) -> StepStatus {
    match status {
        "error" | "interrupted" => StepStatus::Error,
        "done" => StepStatus::Done,
        _ => StepStatus::Running,
    }
}

/// A value when it is a string, else "" (upstream `stringOf`).
pub fn string_of(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

/// A call's details as a JSON object ({}, when absent or an array).
pub fn details_of(value: Option<&serde_json::Value>) -> serde_json::Map<String, serde_json::Value> {
    match value {
        Some(serde_json::Value::Object(map)) => map.clone(),
        _ => serde_json::Map::new(),
    }
}

/// A file path's last part: `src/a.ts` is `a.ts`.
pub fn file_name(path: &str) -> String {
    path.split('/')
        .rfind(|part| !part.is_empty())
        .unwrap_or(path)
        .to_owned()
}

/// A text's first non-empty line, trimmed.
pub fn first_line(text: &str) -> String {
    text.split('\n')
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_owned()
}

/// A text's last line, trimmed: a command's result.
pub fn last_line(text: &str) -> Option<String> {
    text.trim_end()
        .split('\n')
        .next_back()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Markdown as plain words, for a one-line title (upstream `plainText`).
pub fn plain_text(text: &str) -> String {
    // Code spans: `` `x` `` -> `x`.
    let mut out = String::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`'
            && let Some(end) = text[i + 1..].find('`')
        {
            out.push_str(&text[i + 1..i + 1 + end]);
            i = i + 1 + end + 1;
            continue;
        }
        if text[i..].starts_with("**") || text[i..].starts_with("__") {
            let marker = &text[i..i + 2];
            if let Some(end) = text[i + 2..].find(marker) {
                out.push_str(&text[i + 2..i + 2 + end]);
                i = i + 2 + end + 2;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    // Links: `[text](url)` -> `text`.
    let mut final_out = String::new();
    let rest = out.as_str();
    let mut i = 0;
    while i < rest.len() {
        if rest[i..].starts_with('[')
            && let Some(close) = rest[i..].find("](")
            && let Some(end) = rest[i + close + 2..].find(')')
        {
            final_out.push_str(&rest[i + 1..i + close]);
            i += close + 2 + end + 1;
            continue;
        }
        let ch = rest[i..].chars().next().unwrap_or('\0');
        final_out.push(ch);
        i += ch.len_utf8();
    }
    final_out
}

/// The first sentence of a text, for a one-line description.
pub fn sentence(text: &str) -> String {
    let line = first_line(text);
    let chars: Vec<char> = line.chars().collect();
    for index in 0..chars.len() {
        if matches!(chars[index], '.' | '!' | '?')
            && (index + 1 == chars.len()
                || chars.get(index + 1).is_some_and(|ch| ch.is_whitespace()))
        {
            return if index > 0 {
                chars[..=index].iter().collect()
            } else {
                line
            };
        }
    }
    line
}

/// A duration as a reader says it: 12s, 1m 05s, 1h 02m.
pub fn elapsed(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m {:02}s", seconds % 60);
    }
    format!("{}h {:02}m", minutes / 60, minutes % 60)
}

/// A step's state for its row in the sheet.
pub fn step_state(step: &Step, tools: &ToolActivityTable) -> String {
    match step.status {
        StepStatus::Running => "● running".to_owned(),
        StepStatus::Error => {
            let reason = tools
                .get(&step.name)
                .and_then(|tool| tool.failure.as_deref())
                .unwrap_or("failed");
            format!("✗ {reason}")
        }
        StepStatus::Done => match step.duration_ms {
            None => "✓".to_owned(),
            Some(ms) if ms < 1000 => format!("✓ {ms}ms"),
            Some(ms) if ms < 60_000 => format!("✓ {:.1}s", ms as f64 / 1000.0),
            Some(ms) => format!("✓ {}", elapsed(ms)),
        },
    }
}

/// A tool's arguments, parsed when a settled list kept them as JSON text.
pub fn parsed_args(value: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => map.clone(),
        serde_json::Value::String(text) => serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|parsed| parsed.as_object().cloned())
            .unwrap_or_default(),
        _ => serde_json::Map::new(),
    }
}

/// One line for a finished run of calls: the tools' words, counted, in order.
/// `failures: false` leaves out the failures.
pub fn summary(steps: &[Step], tools: &ToolActivityTable, failures: bool) -> String {
    type Count = (String, fn(usize) -> String, usize);
    let mut counts: Vec<Count> = vec![];
    for step in steps {
        let own = tools.get(&step.name).and_then(|tool| tool.summary);
        let name = own
            .map(|function| format!("fn:{}", function as usize))
            .unwrap_or_else(|| step.name.clone());
        match counts.iter_mut().find(|(key, _, _)| *key == name) {
            Some(entry) => entry.2 += 1,
            None => {
                let words = own.unwrap_or(used_tool(&step.name));
                counts.push((name, words, 1));
            }
        }
    }
    let parts: Vec<String> = counts
        .iter()
        .map(|(_, words, count)| words(*count))
        .collect();
    let joined = if parts.is_empty() {
        "done".to_owned()
    } else {
        parts.join(", ")
    };
    let capitalized = {
        let mut chars = joined.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => joined.clone(),
        }
    };
    let errors = if failures {
        steps
            .iter()
            .filter(|step| step.status == StepStatus::Error)
            .count()
    } else {
        0
    };
    if errors > 0 {
        format!("{capitalized} · {errors} failed")
    } else {
        capitalized
    }
}

fn used_tool(name: &str) -> fn(usize) -> String {
    // A closure cannot be returned as a fn pointer without capturing; use a static registry
    // keyed by an interned name instead. See `summary_named` for the general case.
    let _ = name;
    unused_summary
}

fn unused_summary(count: usize) -> String {
    if count == 1 {
        "used a tool".to_owned()
    } else {
        format!("used a tool {count} times")
    }
}

/// The first short string argument of a tool (upstream `shortArgument`): what the generic
/// label names.
pub fn short_argument(args: &serde_json::Map<String, serde_json::Value>) -> String {
    args.values()
        .find_map(|value| value.as_str().filter(|text| !text.trim().is_empty()))
        .map(|text| {
            let first = first_line(text);
            first.chars().take(120).collect::<String>()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_status_mapping() {
        assert_eq!(call_status("pending"), StepStatus::Running);
        assert_eq!(call_status("running"), StepStatus::Running);
        assert_eq!(call_status("done"), StepStatus::Done);
        assert_eq!(call_status("error"), StepStatus::Error);
        assert_eq!(call_status("interrupted"), StepStatus::Error);
    }

    #[test]
    fn text_helpers() {
        assert_eq!(file_name("src/a.ts"), "a.ts");
        assert_eq!(first_line("  \n first \n second"), "first");
        assert_eq!(last_line("a\nb  \n"), Some("b".to_owned()));
        assert_eq!(
            plain_text("a `code` and **bold** and [link](http://x)"),
            "a code and bold and link"
        );
        assert_eq!(sentence("Hello there. More text"), "Hello there.");
    }

    #[test]
    fn elapsed_formats() {
        assert_eq!(elapsed(0), "0s");
        assert_eq!(elapsed(12_000), "12s");
        assert_eq!(elapsed(65_000), "1m 05s");
        assert_eq!(elapsed(3_723_000), "1h 02m");
    }

    #[test]
    fn step_state_formats() {
        let mut step = Step::new("1", "bash", StepStatus::Done);
        assert_eq!(step_state(&step, &ToolActivityTable::new()), "✓");
        step.duration_ms = Some(1500);
        assert_eq!(step_state(&step, &ToolActivityTable::new()), "✓ 1.5s");
        step.status = StepStatus::Error;
        assert_eq!(step_state(&step, &ToolActivityTable::new()), "✗ failed");
    }

    #[test]
    fn summary_groups_by_words_and_counts() {
        fn read(count: usize) -> String {
            if count == 1 {
                "read a file".into()
            } else {
                format!("read {count} files")
            }
        }
        let mut tools = ToolActivityTable::new();
        tools.insert(
            "read".into(),
            ToolActivity {
                summary: Some(read),
                ..Default::default()
            },
        );
        let steps = vec![
            Step::new("1", "read", StepStatus::Done),
            Step::new("2", "read", StepStatus::Done),
            Step::new("3", "bash", StepStatus::Error),
        ];
        assert_eq!(
            summary(&steps, &tools, true),
            "Read 2 files, used a tool · 1 failed"
        );
        assert_eq!(summary(&steps, &tools, false), "Read 2 files, used a tool");
    }

    #[test]
    fn string_and_details_helpers() {
        assert_eq!(string_of(Some(&serde_json::json!("x"))), "x");
        assert_eq!(string_of(Some(&serde_json::json!(3))), "");
        assert!(details_of(Some(&serde_json::json!([]))).is_empty());
        assert_eq!(details_of(Some(&serde_json::json!({"a": 1}))).len(), 1);
        assert_eq!(parsed_args(&serde_json::json!(r#"{"path":"a"}"#)).len(), 1);
    }

    #[test]
    fn default_tool_activities_cover_core_tools() {
        let table = default_tool_activities();
        assert_eq!(
            table.get("bash").and_then(|t| t.brief.as_deref()),
            Some("Running")
        );
        assert_eq!(
            table.get("read").and_then(|t| t.brief.as_deref()),
            Some("Reading")
        );
        assert_eq!(
            table.get("artifact_show").and_then(|t| t.result_placement),
            Some(ResultPlacement::Inline)
        );
        let steps = vec![
            Step::new("1", "bash", StepStatus::Done),
            Step::new("2", "edit", StepStatus::Done),
        ];
        assert_eq!(
            summary(&steps, &table, false),
            "Ran a command, edited a file"
        );
    }
}

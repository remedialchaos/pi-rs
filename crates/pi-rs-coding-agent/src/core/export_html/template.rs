//! Document skeleton and reusable HTML sections for the HTML export.
//!
//! Upstream's `template.html` reads a base64-encoded session blob and renders it
//! in the browser with vendored JavaScript. This port renders the transcript on
//! the server instead, so the result is a single self-contained document: inline
//! CSS, no scripts, no external assets.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::escape_html;

/// The complete HTML document. CSS is inline and every dynamic field has been
/// escaped by the section builders before it reaches this function.
pub fn document_html(
    css: &str,
    title: &str,
    header_html: &str,
    tree_html: &str,
    messages_html: &str,
    footer_html: &str,
) -> String {
    let sidebar = if tree_html.is_empty() {
        String::new()
    } else {
        format!(
            "<nav id=\"sidebar\"><div class=\"sidebar-heading\">Session Tree</div>{tree_html}</nav>\n"
        )
    };
    format!(
        "<!DOCTYPE html>\n\
         <html lang=\"en\">\n\
         <head>\n\
           <meta charset=\"UTF-8\">\n\
           <meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n\
           <title>{}</title>\n\
           <style>\n{}\n</style>\n\
         </head>\n\
         <body>\n\
           <div id=\"app\">\n\
             {}\
             <main id=\"content\">\n\
               <div id=\"header-container\">{}</div>\n\
               <div id=\"messages\">{}</div>\n\
               {}\n\
             </main>\n\
           </div>\n\
         </body>\n\
         </html>\n",
        escape_html(title),
        css,
        sidebar,
        header_html,
        messages_html,
        footer_html,
    )
}

/// Pre-rendered, escaped fields of the statistics header.
pub struct HeaderFields<'a> {
    pub session_id: &'a str,
    pub date: &'a str,
    pub models: &'a str,
    pub messages: &'a str,
    pub tool_calls: usize,
    pub tokens: &'a str,
    pub cost: &'a str,
    /// System prompt / tools blocks appended after the info table.
    pub extra: &'a str,
}

/// Render the session header and cost/token summary.
pub fn render_header_html(fields: &HeaderFields<'_>) -> String {
    let mut html = String::new();
    html.push_str("<div class=\"header\">");
    html.push_str(&format!(
        "<h1>Session: {}</h1>",
        escape_html(fields.session_id)
    ));
    let tool_calls = fields.tool_calls.to_string();
    for (label, value) in [
        ("Date:", fields.date),
        ("Models:", fields.models),
        ("Messages:", fields.messages),
        ("Tool Calls:", &tool_calls),
        ("Tokens:", fields.tokens),
        ("Cost:", fields.cost),
    ] {
        html.push_str(&format!(
            "<div class=\"info-item\"><span class=\"info-label\">{}</span><span class=\"info-value\">{}</span></div>",
            escape_html(label),
            value,
        ));
    }
    html.push_str("</div>");
    html.push_str(fields.extra);
    html
}

/// Render the session tree as a flat list. The traversal is iterative so a
/// single-child chain deeper than the call stack (upstream issue #11225) is
/// handled without recursion.
pub fn render_tree_html(entries: &[Value], leaf_id: Option<&str>) -> String {
    let by_id: HashMap<String, usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            entry
                .get("id")
                .and_then(Value::as_str)
                .map(|id| (id.to_string(), index))
        })
        .collect();

    let mut labels: HashMap<String, String> = HashMap::new();
    for entry in entries {
        if entry.get("type").and_then(Value::as_str) == Some("label")
            && let (Some(target), Some(label)) = (
                entry.get("targetId").and_then(Value::as_str),
                entry.get("label").and_then(Value::as_str),
            )
        {
            labels.insert(target.to_string(), label.to_string());
        }
    }

    // Parent -> children, preserving source order (upstream sorts by timestamp;
    // session entries are already written in timestamp order).
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let parent = entry.get("parentId").and_then(Value::as_str);
        let is_root = match parent {
            None => true,
            Some(parent) => match by_id.get(parent) {
                Some(parent_index) => *parent_index == index,
                None => true,
            },
        };
        if is_root {
            roots.push(index);
        } else if let Some(parent) = parent
            && let Some(parent_index) = by_id.get(parent)
        {
            children[*parent_index].push(index);
        }
    }

    let mut active: HashSet<String> = HashSet::new();
    if let Some(leaf) = leaf_id {
        let mut current = Some(leaf);
        let mut seen: HashSet<String> = HashSet::new();
        while let Some(id) = current {
            if !seen.insert(id.to_string()) {
                break;
            }
            active.insert(id.to_string());
            let entry = by_id.get(id).and_then(|index| entries.get(*index));
            current = entry
                .and_then(|entry| entry.get("parentId"))
                .and_then(Value::as_str)
                .filter(|parent| *parent != id);
        }
    }

    let mut html = String::with_capacity(entries.len() * 120);
    let mut stack: Vec<(usize, bool)> = Vec::new();
    for &root in roots.iter().rev() {
        stack.push((root, true));
    }
    while let Some((index, is_last)) = stack.pop() {
        let Some(entry) = entries.get(index) else {
            continue;
        };
        let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
        let class = if active.contains(id) {
            "tree-node in-path"
        } else {
            "tree-node"
        };
        let marker = if active.contains(id) { "•" } else { " " };
        let display = super::tree_node_display(entry, labels.get(id).map(String::as_str));
        let connector = if roots.contains(&index) {
            String::new()
        } else if is_last {
            "└─ ".to_string()
        } else {
            "├─ ".to_string()
        };
        html.push_str(&format!(
            "<div class=\"{}\" data-id=\"{}\"><span class=\"tree-prefix\">{}</span><span class=\"tree-marker\">{}</span><span class=\"tree-content\">{}</span></div>",
            class,
            escape_html(id),
            escape_html(&connector),
            marker,
            display,
        ));

        let kids = &children[index];
        for (position, child) in kids.iter().enumerate().rev() {
            stack.push((*child, position + 1 == kids.len()));
        }
    }
    if entries.is_empty() {
        html.push_str("<div class=\"tree-node tree-muted\">(empty session)</div>");
    }
    html
}

/// Render a collapsible system prompt block. Long prompts match upstream's
/// ten-line preview/full split.
pub fn render_system_prompt_block(title: &str, text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let title = escape_html(title);
    if lines.len() > 10 {
        let preview = lines[..10].join("\n");
        let remaining = lines.len() - 10;
        format!(
            "<details class=\"system-prompt expandable\">\
               <summary class=\"system-prompt-header\">{title}</summary>\
               <div class=\"system-prompt-preview\">{}</div>\
               <div class=\"system-prompt-expand-hint\">... ({} more lines, click to expand)</div>\
               <div class=\"system-prompt-full\">{}</div>\
             </details>",
            escape_html(&preview),
            remaining,
            escape_html(text),
        )
    } else {
        format!(
            "<div class=\"system-prompt\">\
               <div class=\"system-prompt-header\">{title}</div>\
               <div class=\"system-prompt-full\">{}</div>\
             </div>",
            escape_html(text),
        )
    }
}

/// Render a tool list (available tools, tools added, tools removed).
pub fn render_tools_list(title: &str, tools: &[Value]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut html = format!(
        "<div class=\"tools-list\"><div class=\"tools-header\">{}</div><div class=\"tools-content\">",
        escape_html(title),
    );
    for tool in tools {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let properties = tool
            .get("parameters")
            .and_then(|params| params.get("properties"))
            .and_then(Value::as_object);
        if let Some(properties) = properties
            && !properties.is_empty()
        {
            let required = tool
                .get("parameters")
                .and_then(|params| params.get("required"))
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<HashSet<_>>()
                })
                .unwrap_or_default();
            html.push_str("<details class=\"tool-item\"><summary>");
            html.push_str(&format!(
                "<span class=\"tool-item-name\">{}</span> - <span class=\"tool-item-desc\">{}</span> <span class=\"tool-params-hint\"></span>",
                escape_html(name),
                escape_html(description),
            ));
            html.push_str("</summary><div class=\"tool-params-content\">");
            for (param_name, property) in properties {
                let param_type = property
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("any");
                let required_label = if required.contains(param_name.as_str()) {
                    "<span class=\"tool-param-required\">required</span>"
                } else {
                    "<span class=\"tool-param-optional\">optional</span>"
                };
                html.push_str(&format!(
                    "<div class=\"tool-param\"><span class=\"tool-param-name\">{}</span> \
                     <span class=\"tool-param-type\">{}</span> {}",
                    escape_html(param_name),
                    escape_html(param_type),
                    required_label,
                ));
                if let Some(desc) = property.get("description").and_then(Value::as_str) {
                    html.push_str(&format!(
                        "<div class=\"tool-param-desc\">{}</div>",
                        escape_html(desc),
                    ));
                }
                html.push_str("</div>");
            }
            html.push_str("</div></details>");
        } else {
            html.push_str(&format!(
                "<div class=\"tool-item\"><span class=\"tool-item-name\">{}</span> - \
                 <span class=\"tool-item-desc\">{}</span></div>",
                escape_html(name),
                escape_html(description),
            ));
        }
    }
    html.push_str("</div></div>");
    html
}

/// Render a simple footer with the export marker.
pub fn render_footer_html(summary: &str) -> String {
    format!(
        "<div class=\"footer\">Self-contained HTML export{}</div>",
        if summary.is_empty() {
            String::new()
        } else {
            format!(" - {summary}")
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_is_self_contained() {
        let html = document_html("body {}", "a<b", "", "", "hi", "");
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<style>"));
        assert!(!html.contains("<script"));
        assert!(html.contains("<title>a&lt;b</title>"));
        assert!(html.contains("<div id=\"messages\">hi</div>"));
    }

    #[test]
    fn header_escapes_session_id_and_formats_fields() {
        let fields = HeaderFields {
            session_id: "s\"<&",
            date: "2026-01-01",
            models: "gpt",
            messages: "2 user",
            tool_calls: 3,
            tokens: "1.5k",
            cost: "$0.123",
            extra: "",
        };
        let html = render_header_html(&fields);
        assert!(html.contains("Session: s&quot;&lt;&amp;"));
        assert!(html.contains("<span class=\"info-value\">$0.123</span>"));
        assert!(html.contains("<span class=\"info-value\">3</span>"));
    }

    #[test]
    fn system_prompt_preview_is_capped_at_ten_lines() {
        let text = (1..=12)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let html = render_system_prompt_block("System Prompt", &text);
        assert!(html.contains("class=\"system-prompt expandable\""));
        assert!(html.contains("2 more lines"));
        assert!(html.contains("line 12"));
    }

    #[test]
    fn tree_handles_a_deep_single_child_chain() {
        let mut entries = Vec::new();
        let depth = 5_000;
        for i in 0..depth {
            let mut entry = serde_json::json!({
                "type": "message",
                "id": format!("e{i}"),
                "parentId": if i == 0 { Value::Null } else { Value::String(format!("e{}", i - 1)) },
                "message": { "role": "user", "content": "x" },
            });
            if i == depth - 1 {
                entry["message"]["content"] = Value::String("leaf".to_string());
            }
            entries.push(entry);
        }
        let html = render_tree_html(&entries, Some(&format!("e{}", depth - 1)));
        assert_eq!(html.matches("class=\"tree-node").count(), depth);
        assert!(html.contains("in-path"));
        assert!(html.contains("leaf"));
    }
}

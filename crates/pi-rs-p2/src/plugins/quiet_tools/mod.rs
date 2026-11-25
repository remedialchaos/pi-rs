//! `quiet-tools` built-in plugin: TUI compact one-line tool call summary formatting
//! and `/settings` field (`toolCalls`: `quiet` | `full`).

use pi_rs_chord::{
    Result,
    contracts::Contract,
    host::{Members, ServeOptions, define_facet},
};
use serde_json::{Map, Value};
use std::sync::Arc;

use crate::api::v1::{FacetKind, PluginRegistry};

/// Setting descriptor mirrored from `apps/p2/plugins/quiet-tools/tui.ts`.
pub const TOOL_CALLS_SETTING_PLUGIN: &str = "quiet-tools";
pub const TOOL_CALLS_SETTING_NAME: &str = "toolCalls";
pub const TOOL_CALLS_VALUES: [&str; 2] = ["quiet", "full"];
pub const TOOL_CALLS_DEFAULT: &str = "quiet";

/// First non-empty line of a string, suffixed with ` …` when more non-empty lines follow.
pub fn first_line(value: &str) -> String {
    let lines: Vec<&str> = value
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let head = lines
        .first()
        .copied()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if lines.len() > 1 {
        format!("{head} …")
    } else {
        head
    }
}

/// Settled call duration label when `>= 1000ms`.
pub fn format_duration(ms: Option<u64>) -> String {
    match ms {
        Some(v) if v >= 1000 => format!(" {:.1}s", (v as f64) / 1000.0),
        _ => String::new(),
    }
}

/// Compact summary argument distinguishing calls of a tool.
pub fn summarize_tool_call(
    name: &str,
    args: &Map<String, Value>,
    nested_calls: &[String],
) -> String {
    let str_arg = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or("");
    match name {
        "read" => {
            let path = args
                .get("path")
                .or_else(|| args.get("file_path"))
                .and_then(Value::as_str)
                .unwrap_or("");
            match args.get("offset").and_then(Value::as_u64) {
                Some(offset) => format!("{path}:{offset}"),
                None => path.to_owned(),
            }
        }
        "write" | "edit" => args
            .get("path")
            .or_else(|| args.get("file_path"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        "ls" => {
            let p = str_arg("path");
            if p.is_empty() { ".".into() } else { p.into() }
        }
        "grep" | "find" => {
            let pat = str_arg("pattern");
            let p = str_arg("path");
            if p.is_empty() {
                pat.into()
            } else {
                format!("{pat} in {p}")
            }
        }
        "bash" | "powershell" => first_line(str_arg("command")),
        "codemode" => {
            if nested_calls.is_empty() {
                first_line(str_arg("code"))
            } else {
                let mut unique = Vec::new();
                for call in nested_calls {
                    if !unique.contains(call) {
                        unique.push(call.clone());
                    }
                }
                let plural = if nested_calls.len() == 1 { "" } else { "s" };
                format!("{} call{plural}: {}", nested_calls.len(), unique.join(", "))
            }
        }
        _ => args
            .values()
            .find_map(|v| v.as_str().filter(|s| !s.trim().is_empty()))
            .map(first_line)
            .unwrap_or_default(),
    }
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let contract = Arc::new(Contract::local(
        "p2.tui-quiet-tools",
        1,
        Vec::new(),
        indexmap::IndexMap::new(),
    )?);
    registry.register(
        "quiet-tools",
        FacetKind::Tui,
        define_facet(move |host| {
            host.serve(
                contract.clone(),
                |_, _, _| Ok(Members::new()),
                ServeOptions::default(),
            )
        }),
    )
}

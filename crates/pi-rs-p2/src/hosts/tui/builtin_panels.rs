//! Built-in TUI slash commands and side panels wired to native `p2` server/worker contracts.
use super::view::View;
use pi_rs_p2_tui::{
    RenderContext, ThemeRef,
    components::{PanelContent, PanelFrame},
    line_text,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActivePanel {
    Help,
    Tree,
    Models,
    Cost,
    Status,
    Themes,
}

pub(super) struct SlashOutcome {
    pub(super) handled: bool,
    pub(super) panel: Option<PanelFrame>,
    pub(super) flash: Option<String>,
}

struct Lines(Vec<String>);
impl pi_rs_tui::Component for Lines {
    fn render(&self, _: usize) -> Vec<String> {
        self.0.clone()
    }
}
impl PanelContent for Lines {}

struct Style(ThemeRef);
impl RenderContext for Style {
    fn theme(&self) -> &ThemeRef {
        &self.0
    }
    fn key_text(&self, id: &str) -> String {
        if id == "p2.panel.focus" {
            "F2".into()
        } else {
            "Esc".into()
        }
    }
    fn request_render(&self) {}
}

#[derive(Default)]
pub(super) struct BuiltinPanels {
    active: Option<ActivePanel>,
    current_theme: String,
    available_themes: Vec<String>,
}

impl BuiltinPanels {
    pub(super) fn set_theme_info(&mut self, current: String, available: Vec<String>) {
        self.current_theme = current;
        self.available_themes = available;
    }

    pub(super) fn close(&mut self) {
        self.active = None;
    }

    pub(super) fn on_session_change(&mut self) {
        self.active = None;
    }

    pub(super) fn handle_slash(
        &mut self,
        name: &str,
        args: &str,
        view: &mut View,
        theme: &ThemeRef,
    ) -> SlashOutcome {
        let mut flash = None;
        match name {
            "help" => {
                self.active = Some(ActivePanel::Help);
            }
            "new" => {
                let title = (!args.is_empty()).then_some(args);
                if let Err(err) = view.create_session(title) {
                    flash = Some(line_text(&err.to_string()));
                }
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    flash,
                };
            }
            "sessions" => {
                if let Err(err) = view.choose("open") {
                    flash = Some(line_text(&err.to_string()));
                }
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    flash,
                };
            }
            "tree" => {
                if let Err(err) = view.request_tree() {
                    flash = Some(line_text(&err.to_string()));
                }
                self.active = Some(ActivePanel::Tree);
            }
            "models" => {
                if let Err(err) = view.request_models() {
                    flash = Some(line_text(&err.to_string()));
                }
                self.active = Some(ActivePanel::Models);
            }
            "cost" => {
                if let Err(err) = view.request_costs() {
                    flash = Some(line_text(&err.to_string()));
                }
                self.active = Some(ActivePanel::Cost);
            }
            "status" => {
                if let Err(err) = view.request_status() {
                    flash = Some(line_text(&err.to_string()));
                }
                self.active = Some(ActivePanel::Status);
            }
            "theme" => {
                self.active = Some(ActivePanel::Themes);
            }
            _ => {
                return SlashOutcome {
                    handled: false,
                    panel: None,
                    flash: None,
                };
            }
        }
        SlashOutcome {
            handled: true,
            panel: self.refresh(view, theme),
            flash,
        }
    }

    pub(super) fn refresh(&self, view: &View, theme: &ThemeRef) -> Option<PanelFrame> {
        let active = self.active?;
        let (id, title, lines) = match active {
            ActivePanel::Help => ("builtin.help", "Help", render_help(theme)),
            ActivePanel::Tree => (
                "builtin.tree",
                "Conversation Tree",
                render_tree(view, theme),
            ),
            ActivePanel::Models => ("builtin.models", "Models", render_models(view, theme)),
            ActivePanel::Cost => ("builtin.cost", "Session Costs", render_cost(view, theme)),
            ActivePanel::Status => (
                "builtin.status",
                "Session Status",
                render_status(view, theme),
            ),
            ActivePanel::Themes => (
                "builtin.themes",
                "Themes",
                render_themes(&self.current_theme, &self.available_themes, theme),
            ),
        };
        let title = title.to_owned();
        Some(PanelFrame::new(
            Box::new(Style(theme.clone())),
            id.into(),
            Box::new(Lines(lines)),
            move || title.clone(),
        ))
    }
}

fn render_help(theme_ref: &ThemeRef) -> Vec<String> {
    let theme = theme_ref.current();
    let mut lines = vec![theme.bold(&theme.fg("accent", "Built-in Commands"))];
    for (cmd, desc) in [
        ("/new [title]", "Create and switch to a new session"),
        ("/sessions", "Open the session picker"),
        ("/tree", "Show session conversation tree"),
        ("/models", "List configured models"),
        ("/cost", "Show token & cost breakdown"),
        ("/status", "Show active workers & session status"),
        ("/theme [name]", "List or switch TUI color theme"),
        ("/help", "Show this help panel"),
        ("/close-panel", "Close the side panel"),
    ] {
        lines.push(format!(
            "  {}  {}",
            theme.fg("accent", cmd),
            theme.fg("text", desc)
        ));
    }
    lines.push(String::new());
    lines.push(theme.bold(&theme.fg("accent", "Keybindings")));
    for (key, desc) in [
        ("Ctrl+R", "Open session picker"),
        ("F2", "Toggle side panel focus"),
        ("Esc", "Return focus to editor"),
        ("PgUp / PgDn", "Scroll focused panel"),
        ("Ctrl+C", "Quit"),
    ] {
        lines.push(format!(
            "  {}  {}",
            theme.fg("dim", key),
            theme.fg("text", desc)
        ));
    }
    lines
}

fn render_models(view: &View, theme_ref: &ThemeRef) -> Vec<String> {
    let theme = theme_ref.current();
    if let Some(err) = &view.models_error {
        return vec![theme.fg("error", err)];
    }
    if !view.models_ready {
        return vec![theme.fg("muted", "Loading models…")];
    }
    let mut lines = Vec::new();
    let default_provider = view.default_model["provider"].as_str();
    let default_id = view.default_model["modelId"].as_str();
    if let (Some(provider), Some(id)) = (default_provider, default_id) {
        let thinking = view.default_model["thinkingLevel"]
            .as_str()
            .map(|t| format!(" ({t})"))
            .unwrap_or_default();
        lines.push(format!(
            "{} {}{}",
            theme.fg("muted", "Default:"),
            theme.fg("accent", &format!("{provider}/{id}")),
            theme.fg("dim", &thinking)
        ));
        lines.push(String::new());
    }
    lines.push(theme.bold(&theme.fg(
        "text",
        &format!("Configured Models ({})", view.models_list.len()),
    )));
    if view.models_list.is_empty() {
        lines.push(theme.fg("muted", "  No models configured."));
    } else {
        for model in &view.models_list {
            let provider = model["provider"].as_str().unwrap_or("unknown");
            let id = model["id"]
                .as_str()
                .or_else(|| model["modelId"].as_str())
                .unwrap_or("unknown");
            let is_default = default_provider == Some(provider) && default_id == Some(id);
            let marker = if is_default {
                theme.fg("accent", "● ")
            } else {
                "  ".to_owned()
            };
            let label = format!("{provider}/{id}");
            let styled = if is_default {
                theme.bold(&theme.fg("accent", &label))
            } else {
                theme.fg("text", &label)
            };
            let ctx = model["contextWindow"]
                .as_u64()
                .map(|w| format!("  {}", theme.fg("dim", &format!("{w} ctx"))))
                .unwrap_or_default();
            lines.push(format!("{marker}{styled}{ctx}"));
        }
    }
    if !view.models_errors.is_empty() {
        lines.push(String::new());
        lines.push(theme.fg("error", "Errors:"));
        for err in &view.models_errors {
            let msg = err
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| err.to_string());
            lines.push(theme.fg("error", &format!("  {msg}")));
        }
    }
    lines
}

fn render_tree(view: &View, theme_ref: &ThemeRef) -> Vec<String> {
    let theme = theme_ref.current();
    if let Some(err) = &view.tree_error {
        return vec![theme.fg("error", err)];
    }
    let Some(tree) = &view.tree_data else {
        return vec![theme.fg("muted", "Loading conversation tree…")];
    };
    let main = tree["main"].as_u64().unwrap_or(1);
    let current = tree["current"].as_u64();
    let mut labels: HashMap<u64, String> = HashMap::new();
    for branch in tree["branches"].as_array().into_iter().flatten() {
        if let (Some(conv), Some(label)) =
            (branch["conversation"].as_u64(), branch["label"].as_str())
        {
            labels.insert(conv, label.to_owned());
        }
    }
    let mut lines = vec![format!("{} #{}", theme.fg("muted", "Main branch:"), main)];
    if let Some(label) = labels.get(&main) {
        lines.push(format!("{} {}", theme.fg("muted", "Label:"), label));
    }
    lines.push(String::new());

    let nodes: Vec<&Value> = tree["nodes"].as_array().into_iter().flatten().collect();
    if nodes.is_empty() {
        lines.push(theme.fg("muted", "No messages in this session yet."));
        return lines;
    }

    let ids: HashSet<u64> = nodes.iter().filter_map(|n| n["entry"].as_u64()).collect();
    let mut children: HashMap<Option<u64>, Vec<&Value>> = HashMap::new();
    let mut parents: HashMap<u64, Option<u64>> = HashMap::new();
    for node in &nodes {
        let Some(entry) = node["entry"].as_u64() else {
            continue;
        };
        let parent = node["parent"].as_u64().filter(|p| ids.contains(p));
        parents.insert(entry, parent);
        children.entry(parent).or_default().push(*node);
    }
    let mut active_path = HashSet::new();
    let mut cursor = current;
    while let Some(id) = cursor {
        if !active_path.insert(id) {
            break;
        }
        cursor = parents.get(&id).copied().flatten();
    }

    let mut seen = HashSet::new();
    let roots = children.get(&None).map(Vec::as_slice).unwrap_or(&[]);
    let mut stack: Vec<(&Value, String, Option<bool>, bool)> = roots
        .iter()
        .rev()
        .map(|n| (*n, String::new(), None, roots.len() > 1))
        .collect();
    while let Some((node, mut gutter, connector, virtual_root)) = stack.pop() {
        let Some(entry) = node["entry"].as_u64() else {
            continue;
        };
        if !seen.insert(entry) {
            continue;
        }
        let prefix = format!(
            "{gutter}{}",
            match connector {
                Some(true) => "└─ ",
                Some(false) => "├─ ",
                None => "",
            }
        );
        let role = node["role"].as_str().unwrap_or("user");
        let role_tag = if role == "user" {
            theme.fg("accent", "user: ")
        } else {
            theme.fg("muted", "assistant: ")
        };
        let active_dot = if active_path.contains(&entry) {
            theme.fg("accent", "• ")
        } else {
            String::new()
        };
        let preview = line_text(node["text"].as_str().unwrap_or(""));
        lines.push(format!(
            "{}{}{role_tag}{preview}",
            theme.fg("dim", &prefix),
            active_dot
        ));
        if let Some(last) = connector {
            gutter.push_str(if last { "   " } else { "│  " });
        }
        if let Some(kids) = children.get(&Some(entry)) {
            let branches = kids.len() > 1;
            if !branches && (connector.is_some() || virtual_root) {
                gutter.push_str("   ");
            }
            for (i, kid) in kids.iter().enumerate().rev() {
                stack.push((
                    *kid,
                    gutter.clone(),
                    branches.then_some(i + 1 == kids.len()),
                    false,
                ));
            }
        }
    }
    lines
}

fn render_cost(view: &View, theme_ref: &ThemeRef) -> Vec<String> {
    let theme = theme_ref.current();
    if let Some(err) = &view.costs_error {
        return vec![theme.fg("error", err)];
    }
    let mut lines = Vec::new();
    if let Some(stats) = &view.stats_data {
        let tokens = &stats["tokens"];
        if !tokens.is_null() {
            let input = tokens["input"].as_u64().unwrap_or(0);
            let output = tokens["output"].as_u64().unwrap_or(0);
            let cache_read = tokens["cacheRead"].as_u64().unwrap_or(0);
            let cache_write = tokens["cacheWrite"].as_u64().unwrap_or(0);
            lines.push(theme.bold(&theme.fg("text", "Token Totals")));
            lines.push(format!(
                "  in: {}  out: {}  cacheR: {}  cacheW: {}",
                input, output, cache_read, cache_write
            ));
            if let Some(total_cost) = stats["cost"].as_f64() {
                lines.push(format!(
                    "  {} ${:.4}",
                    theme.fg("muted", "Total cost:"),
                    total_cost
                ));
            }
            lines.push(String::new());
        }
    }
    let Some(costs) = &view.costs_data else {
        if lines.is_empty() {
            lines.push(theme.fg("muted", "Loading session costs…"));
        }
        return lines;
    };
    let rows = costs.as_array().map(Vec::as_slice).unwrap_or(&[]);
    if rows.is_empty() {
        lines.push(theme.fg("muted", "No model responses recorded yet."));
        return lines;
    }
    let total: f64 = rows.iter().filter_map(|r| r["cost"].as_f64()).sum();
    lines.push(theme.bold(&theme.fg(
        "accent",
        &format!("Responses ({})  Total: ${total:.4}", rows.len()),
    )));
    for row in rows {
        let id = row["entryId"].as_str().unwrap_or("?");
        let provider = row["provider"].as_str().unwrap_or("?");
        let model = row["model"].as_str().unwrap_or("?");
        let cost = row["cost"].as_f64().unwrap_or(0.0);
        let cache = row["cacheHit"]
            .as_f64()
            .map(|c| format!("  cache {:.0}%", c * 100.0))
            .unwrap_or_default();
        lines.push(format!(
            "  #{} {}/{}  ${:.4}{}",
            id,
            provider,
            model,
            cost,
            theme.fg("dim", &cache)
        ));
    }
    lines
}

fn render_status(view: &View, theme_ref: &ThemeRef) -> Vec<String> {
    let theme = theme_ref.current();
    if let Some(err) = &view.status_error {
        return vec![theme.fg("error", err)];
    }
    let mut lines = Vec::new();
    if let Some((id, conv)) = view.destination() {
        lines.push(format!(
            "{} {} (#{})",
            theme.fg("muted", "Current:"),
            theme.fg("accent", view.title()),
            conv
        ));
        lines.push(theme.fg("dim", &format!("  session: {id}")));
        lines.push(String::new());
    }
    let Some(statuses) = &view.statuses else {
        lines.push(theme.fg("muted", "Loading session status…"));
        return lines;
    };
    let Some(map) = statuses.as_object() else {
        lines.push(theme.fg("muted", "No active workers."));
        return lines;
    };
    if map.is_empty() {
        lines.push(theme.fg("muted", "No active workers."));
        return lines;
    }
    let current_id = view.destination().map(|(id, _)| id.as_str());
    lines.push(theme.bold(&theme.fg("text", &format!("Workers ({})", map.len()))));
    for (session_id, entry) in map {
        let running = entry["running"].as_bool().unwrap_or(false);
        let tasks = entry["tasks"].as_u64().unwrap_or(0);
        let marker = if current_id == Some(session_id.as_str()) {
            theme.fg("accent", "● ")
        } else {
            "  ".to_owned()
        };
        let state = if running {
            theme.fg("accent", "running")
        } else {
            theme.fg("muted", "idle")
        };
        lines.push(format!("{marker}{session_id}: {state} (tasks: {tasks})"));
    }
    lines
}

fn render_themes(current: &str, available: &[String], theme_ref: &ThemeRef) -> Vec<String> {
    let theme = theme_ref.current();
    let mut lines = vec![theme.bold(&theme.fg("accent", "Available Themes"))];
    lines.push(String::new());
    for name in available {
        let is_current = name == current;
        let marker = if is_current {
            theme.fg("accent", "● ")
        } else {
            "  ".to_owned()
        };
        let label = if is_current {
            theme.bold(&theme.fg("accent", name))
        } else {
            theme.fg("text", name)
        };
        lines.push(format!("{marker}{label}"));
    }
    lines.push(String::new());
    lines.push(theme.fg("dim", "Use /theme <name> to switch."));
    lines
}

//! Interactive TUI facets for the ~20 built-in `p2` plugins, replacing static line panels
//! with interactive list navigation, actions, and overlays.

use super::facets::{
    ActionQueue, FramedOverlay, InteractiveListPanel, InteractiveRow, PanelAction, PanelRenderCtx,
    make_fork_overlay, make_scoped_models_overlay, push_action,
};
use super::view::View;
use pi_rs_p2_tui::{
    ThemeRef,
    components::{ListAction, ListOverlay, ListRow, PanelFrame, RowColor, SelectorContext},
    line_text,
};
use pi_rs_tui::Component;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum ActivePanel {
    Help,
    Sessions,
    Tree,
    Models,
    Cost,
    Status,
    SessionInfo,
    Themes,
    Login,
    Mcp,
    Sharing,
    Devices,
    Envs,
    Ports,
    Artifacts,
    ChatMode,
    Logs(Option<String>),
    Notices,
    Downloads,
    Agents,
    Files(String),
    Changes,
    Grep(String),
    Discord,
    Prompts,
}

pub(super) struct SlashOutcome {
    pub(super) handled: bool,
    pub(super) panel: Option<PanelFrame>,
    pub(super) overlay: Option<(String, Box<dyn Component>, usize)>,
    pub(super) flash: Option<String>,
}

pub(super) struct BuiltinPanels {
    active: Option<ActivePanel>,
    selected: Rc<RefCell<usize>>,
    actions: ActionQueue,
    current_theme: String,
    available_themes: Vec<String>,
}

impl BuiltinPanels {
    pub(super) fn new(actions: ActionQueue) -> Self {
        Self {
            active: None,
            selected: Rc::new(RefCell::new(0)),
            actions,
            current_theme: "system".to_string(),
            available_themes: vec!["dark".into(), "light".into(), "system".into()],
        }
    }

    pub(super) fn set_theme_info(&mut self, current: String, available: Vec<String>) {
        self.current_theme = current;
        self.available_themes = available;
    }

    pub(super) fn close(&mut self) {
        self.active = None;
        *self.selected.borrow_mut() = 0;
    }

    pub(super) fn on_session_change(&mut self) {
        self.active = None;
        *self.selected.borrow_mut() = 0;
    }

    pub(super) fn all_slash_metadata() -> Vec<(&'static str, &'static str, Option<&'static str>)> {
        vec![
            ("help", "Show built-in commands & shortcuts", None),
            ("hotkeys", "Show full keyboard shortcut map", None),
            ("settings", "Configure TUI preferences", None),
            ("keybindings", "Rebind keyboard shortcuts", None),
            ("quit", "Exit the p2 TUI launcher", None),
            ("reload", "Reload remote TUI plugins", None),
            ("close-panel", "Close the active side panel", None),
            ("sessions", "Show and switch sessions", None),
            ("resume", "Interactive session picker", None),
            ("new", "Create a new session", Some("[[env:]path|title]")),
            (
                "cd",
                "Relocate session working directory",
                Some("[[env:]path]"),
            ),
            ("move", "Move session to another environment", Some("[env]")),
            ("name", "Rename current session", Some("<title>")),
            ("dismiss-failed", "Archive failed sessions", None),
            ("models", "List and pick configured models", None),
            ("model", "Switch default model", Some("[provider/model]")),
            (
                "thinking",
                "Set thinking level",
                Some("[off|minimal|low|medium|high|xhigh]"),
            ),
            ("scoped-models", "Configure scoped models", None),
            ("login", "Sign in to model provider", Some("[provider]")),
            ("logout", "Sign out of model provider", Some("[provider]")),
            ("mcp", "Inspect or add MCP servers", Some("[paste <url>]")),
            (
                "share",
                "Manage share links & guests",
                Some("[email [full]]"),
            ),
            ("devices", "Manage paired devices & tokens", None),
            ("envs", "Inspect & test remote environments", None),
            (
                "ports",
                "Manage forwarded ports",
                Some("[open <port> [name] [public]]"),
            ),
            ("artifacts", "Browse session artifacts", None),
            (
                "chat-mode",
                "Toggle or configure chat mode",
                Some("[on|off|<name>]"),
            ),
            ("reply", "Send a reply in chat mode", Some("<text>")),
            ("logs", "Inspect server logs", Some("[source]")),
            ("notices", "Inspect and dismiss notices", None),
            ("reindex", "Rebuild search index", None),
            ("downloads", "Inspect downloadable files", None),
            ("status", "Show active workers & session status", None),
            ("session", "Show active session details", None),
            ("retry-worker", "Restart session worker", None),
            ("dev-reload", "Reload plugins in dev mode", None),
            ("dev-restart", "Restart active worker", None),
            ("agents", "Inspect & switch subagents", None),
            ("main", "Return to parent main session", None),
            ("files", "Browse workspace files", Some("[directory]")),
            ("changes", "View changed files & diffs", None),
            ("grep", "Search workspace text", Some("<text>")),
            ("discord", "Manage Discord bots & bindings", None),
            ("tree", "Navigate conversation tree", None),
            ("fork", "Fork session from a user message", None),
            (
                "compact",
                "Compact conversation context",
                Some("[instructions]"),
            ),
            ("new-context", "Start fresh context", Some("[handoff]")),
            ("clone", "Clone active session", None),
            ("attach", "Attach local path hint", Some("[path]")),
            ("detach", "Detach active panel/attachment", None),
            ("copy", "Copy last assistant response", None),
            ("cost", "Show token & cost breakdown", None),
            ("theme", "List or switch TUI color theme", Some("[name]")),
            ("prompts", "List prompt templates & skills", None),
        ]
    }

    pub(super) fn open_resume_overlay(&self, view: &View, theme: &ThemeRef) -> Box<dyn Component> {
        let current_id = view.destination().map(|(id, _)| id.as_str());
        let rows: Vec<ListRow> = view
            .catalog_rows()
            .iter()
            .filter(|r| r["archivedAt"].is_null())
            .filter_map(|r| {
                let id = r["id"].as_str()?;
                let title = line_text(r["title"].as_str().unwrap_or(id));
                let marker = if current_id == Some(id) { "● " } else { "" };
                let env_part = r["env"]
                    .as_str()
                    .map(|e| format!("{e}:"))
                    .unwrap_or_default();
                let cwd = r["cwd"].as_str().unwrap_or("");
                let detail = if cwd.is_empty() {
                    None
                } else {
                    Some(format!(" {env_part}{cwd}"))
                };
                Some(ListRow {
                    key: id.to_owned(),
                    label: format!("{marker}{title}"),
                    detail,
                    description: None,
                    color: (current_id == Some(id)).then_some(RowColor::Accent),
                })
            })
            .collect();

        let actions_select = self.actions.clone();
        let actions_archive = self.actions.clone();
        let actions_close = self.actions.clone();
        let ctx = SelectorContext::new(
            Box::new(PanelRenderCtx(theme.clone())),
            Box::new(super::facets::PanelKeys),
        );
        let overlay = ListOverlay::new(
            ctx,
            rows,
            vec![ListAction {
                id: None,
                key: Some("ctrl+a".into()),
                label: "archive".into(),
                run: Box::new(move |row| {
                    if let Some(row) = row {
                        push_action(
                            &actions_archive,
                            PanelAction::ArchiveSession(row.key.clone()),
                        );
                    }
                }),
            }],
            move || {
                push_action(&actions_close, PanelAction::CloseOverlay);
            },
        )
        .with_on_select(move |row| {
            push_action(&actions_select, PanelAction::SwitchSession(row.key.clone()));
            push_action(&actions_select, PanelAction::CloseOverlay);
        });
        Box::new(FramedOverlay::new(
            "Sessions (Enter switch · Ctrl+A archive)",
            Box::new(overlay),
            theme.clone(),
        ))
    }

    pub(super) fn handle_slash(
        &mut self,
        name: &str,
        args: &str,
        view: &mut View,
        theme: &ThemeRef,
    ) -> SlashOutcome {
        let mut flash = None;
        let mut overlay = None;
        *self.selected.borrow_mut() = 0;

        match name {
            "help" => {
                self.active = Some(ActivePanel::Help);
            }
            "sessions" => {
                self.active = Some(ActivePanel::Sessions);
            }
            "resume" => {
                overlay = Some((
                    "resume".into(),
                    self.open_resume_overlay(view, theme),
                    75usize,
                ));
            }
            "new" => {
                let trimmed = args.trim();
                let res = if trimmed.is_empty() {
                    view.create_session(None)
                } else if let Some((env, path)) = parse_env_path(trimmed) {
                    view.create_session_at(env.as_deref(), Some(&path), None)
                } else if is_path_like(trimmed) {
                    view.create_session_at(None, Some(trimmed), None)
                } else {
                    view.create_session(Some(trimmed))
                };
                if let Err(err) = res {
                    flash = Some(line_text(&err.to_string()));
                }
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay: None,
                    flash,
                };
            }
            "cd" => {
                let Some((sid, _)) = view.destination().cloned() else {
                    return SlashOutcome {
                        handled: true,
                        panel: None,
                        overlay: None,
                        flash: Some("No active session".into()),
                    };
                };
                let (env_opt, cwd) = parse_env_path(args.trim()).unwrap_or_else(|| {
                    (
                        view.active_env(),
                        if args.trim().is_empty() {
                            view.active_cwd()
                        } else {
                            resolve_relative_path(&view.active_cwd(), args.trim())
                        },
                    )
                });
                let mut payload = json!({ "session": sid, "cwd": cwd });
                if let Some(env_val) = env_opt.filter(|e| e != "local") {
                    payload["env"] = json!(env_val);
                }
                if let Err(err) = view.call_session_op("relocate", payload) {
                    flash = Some(line_text(&err.to_string()));
                }
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay: None,
                    flash,
                };
            }
            "move" => {
                let Some((sid, _)) = view.destination().cloned() else {
                    return SlashOutcome {
                        handled: true,
                        panel: None,
                        overlay: None,
                        flash: Some("No active session".into()),
                    };
                };
                let mut payload = json!({ "session": sid, "cwd": view.active_cwd() });
                let env_name = args.trim();
                if !env_name.is_empty() && env_name != "local" {
                    payload["env"] = json!(env_name);
                }
                if let Err(err) = view.call_session_op("relocate", payload) {
                    flash = Some(line_text(&err.to_string()));
                }
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay: None,
                    flash,
                };
            }
            "name" => {
                let title = args.trim();
                if title.is_empty() {
                    flash = Some("Usage: /name <title>".into());
                } else if let Some((sid, _)) = view.destination().cloned()
                    && let Err(err) =
                        view.call_session_op("rename", json!({ "session": sid, "title": title }))
                {
                    flash = Some(line_text(&err.to_string()));
                }
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay: None,
                    flash,
                };
            }
            "dismiss-failed" => {
                let failed: Vec<String> = view
                    .catalog_rows()
                    .iter()
                    .filter(|r| {
                        r["archivedAt"].is_null()
                            && (!r["error"].is_null() || r["status"].as_str() == Some("failed"))
                    })
                    .filter_map(|r| r["id"].as_str().map(str::to_owned))
                    .collect();
                let count = failed.len();
                for id in failed {
                    let _ = view.call_session_op("archive", json!({ "session": id }));
                }
                flash = Some(format!("Dismissed {count} failed session(s)"));
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay: None,
                    flash,
                };
            }
            "tree" => {
                if let Err(err) = view.request_tree() {
                    flash = Some(line_text(&err.to_string()));
                }
                self.active = Some(ActivePanel::Tree);
            }
            "fork" => {
                if let Some(comp) = make_fork_overlay(view, theme, &self.actions) {
                    overlay = Some(("fork".into(), Box::new(comp), 75));
                } else {
                    flash = Some("No user messages to fork from".into());
                }
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay,
                    flash,
                };
            }
            "models" => {
                if let Err(err) = view.request_models() {
                    flash = Some(line_text(&err.to_string()));
                }
                self.active = Some(ActivePanel::Models);
            }
            "model" => {
                let _ = view.request_models();
                let target = args.trim();
                if let Some((provider, model_id)) = target.split_once('/') {
                    if let Err(err) = view.set_default_model(provider.trim(), model_id.trim()) {
                        flash = Some(line_text(&err.to_string()));
                    } else {
                        let _ = view.configure_chat(json!({
                            "model": { "provider": provider.trim(), "modelId": model_id.trim() }
                        }));
                        flash = Some(format!("Model: {provider}/{model_id}"));
                    }
                    return SlashOutcome {
                        handled: true,
                        panel: None,
                        overlay: None,
                        flash,
                    };
                }
                overlay = Some((
                    "model".into(),
                    self.make_model_list_overlay(view, theme),
                    75,
                ));
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay,
                    flash,
                };
            }
            "thinking" => {
                let arg = args.trim();
                if !arg.is_empty() {
                    if let Err(err) = view.configure_chat(json!({ "thinkingLevel": arg })) {
                        flash = Some(line_text(&err.to_string()));
                    } else {
                        flash = Some(format!("Thinking: {arg}"));
                    }
                    return SlashOutcome {
                        handled: true,
                        panel: None,
                        overlay: None,
                        flash,
                    };
                }
                overlay = Some((
                    "thinking".into(),
                    self.make_thinking_list_overlay(view, theme),
                    60,
                ));
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay,
                    flash,
                };
            }
            "scoped-models" => {
                let _ = view.request_models();
                overlay = Some((
                    "scoped-models".into(),
                    Box::new(make_scoped_models_overlay(view, theme)),
                    75,
                ));
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay,
                    flash,
                };
            }
            "login" => {
                let _ = view.ensure_hold("p2.provider-login", json!({}), "server", json!({}));
                let prov = args.trim();
                if !prov.is_empty() {
                    let _ = view.call_dynamic(
                        "p2.provider-login",
                        "start",
                        json!({ "provider": prov }),
                    );
                    flash = Some(format!("Starting login for {prov}…"));
                }
                self.active = Some(ActivePanel::Login);
            }
            "logout" => {
                let _ = view.ensure_hold("p2.provider-login", json!({}), "server", json!({}));
                let prov = args.trim();
                if !prov.is_empty() {
                    if let Err(err) = view.call_dynamic(
                        "p2.provider-login",
                        "logout",
                        json!({ "provider": prov }),
                    ) {
                        flash = Some(line_text(&err.to_string()));
                    } else {
                        flash = Some(format!("Signed out of {prov}"));
                    }
                } else {
                    self.active = Some(ActivePanel::Login);
                }
            }
            "mcp" => {
                let _ = view.ensure_hold("p2.mcp", json!({}), "server", json!({}));
                if let Some(url) = args.trim().strip_prefix("paste ") {
                    let _ = view.call_dynamic("p2.mcp", "paste", json!({ "url": url.trim() }));
                    flash = Some("Connecting MCP server…".into());
                }
                self.active = Some(ActivePanel::Mcp);
            }
            "share" => {
                if let Some((sid, _)) = view.destination().cloned() {
                    let _ = view.ensure_hold(
                        "p2.sharing",
                        json!({ "session": sid }),
                        "server",
                        json!({}),
                    );
                    let parts: Vec<&str> = args.split_whitespace().collect();
                    if let Some(email) = parts.first().copied() {
                        let full = parts.get(1).copied() == Some("full");
                        let _ = view.call_dynamic(
                            "p2.sharing",
                            "share",
                            json!({ "email": email, "full": full }),
                        );
                        flash = Some(format!("Shared with {email}"));
                    }
                }
                self.active = Some(ActivePanel::Sharing);
            }
            "devices" => {
                let _ = view.ensure_hold("p2.devices", json!({}), "server", json!({}));
                self.active = Some(ActivePanel::Devices);
            }
            "envs" => {
                let _ = view.ensure_hold("p2.envs", json!({}), "server", json!({}));
                let _ = view.call_dynamic("p2.envs", "list", json!({}));
                self.active = Some(ActivePanel::Envs);
            }
            "ports" => {
                let _ = view.ensure_hold("p2.ports", json!({}), "server", json!({}));
                let parts: Vec<&str> = args.split_whitespace().collect();
                if parts.first().copied() == Some("open")
                    && let Some(port) = parts.get(1).and_then(|p| p.parse::<u16>().ok())
                {
                    let label = parts.get(2).copied().unwrap_or("");
                    let public = parts.get(3).copied() == Some("public");
                    let _ = view.call_dynamic(
                        "p2.ports",
                        "add",
                        json!({ "port": port, "name": label, "public": public }),
                    );
                    flash = Some(format!("Opening port {port}"));
                }
                self.active = Some(ActivePanel::Ports);
            }
            "artifacts" => {
                if let Some((sid, _)) = view.destination().cloned() {
                    let _ = view.ensure_hold(
                        "p2.artifacts",
                        json!({ "session": sid }),
                        "server",
                        json!({}),
                    );
                }
                self.active = Some(ActivePanel::Artifacts);
            }
            "chat-mode" => {
                if let Some((sid, conv)) = view.destination().cloned() {
                    let _ = view.ensure_hold(
                        "p2.chat-mode",
                        json!({ "session": sid, "conversation": conv }),
                        "session",
                        json!({ "id": sid }),
                    );
                    let arg = args.trim();
                    if matches!(arg, "on" | "true") {
                        let _ =
                            view.call_dynamic("p2.chat-mode", "set", json!({ "enabled": true }));
                    } else if matches!(arg, "off" | "false") {
                        let _ =
                            view.call_dynamic("p2.chat-mode", "set", json!({ "enabled": false }));
                    } else if !arg.is_empty() {
                        let _ = view.call_dynamic("p2.chat-mode", "rename", json!({ "name": arg }));
                    }
                }
                self.active = Some(ActivePanel::ChatMode);
            }
            "reply" => {
                if args.trim().is_empty() {
                    flash = Some("Usage: /reply <message>".into());
                } else if let Err(err) = view.submit(args.trim()) {
                    flash = Some(line_text(&err.to_string()));
                }
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay: None,
                    flash,
                };
            }
            "logs" => {
                let source = (!args.trim().is_empty()).then(|| args.trim().to_owned());
                let params = match &source {
                    Some(s) => json!({ "source": s }),
                    None => json!({}),
                };
                let _ = view.ensure_hold("p2.logs", params, "server", json!({}));
                self.active = Some(ActivePanel::Logs(source));
            }
            "notices" => {
                let _ = view.ensure_hold("p2.log-notices", json!({}), "server", json!({}));
                self.active = Some(ActivePanel::Notices);
            }
            "reindex" => {
                let _ = view.ensure_hold("p2.search", json!({}), "server", json!({}));
                let _ = view.call_dynamic("p2.search", "rebuildIndex", json!({}));
                flash = Some("Rebuilding search index…".into());
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay: None,
                    flash,
                };
            }
            "downloads" => {
                if let Some((sid, _)) = view.destination().cloned() {
                    let _ = view.ensure_hold(
                        "p2.sent-files",
                        json!({ "session": sid }),
                        "session",
                        json!({ "id": sid }),
                    );
                }
                self.active = Some(ActivePanel::Downloads);
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
            "session" => {
                self.active = Some(ActivePanel::SessionInfo);
            }
            "retry-worker" | "dev-restart" => {
                if let Some((sid, _)) = view.destination().cloned() {
                    let _ = view.ensure_hold(
                        "p2.worker-control",
                        json!({ "session": sid }),
                        "server",
                        json!({}),
                    );
                    let _ = view.call_dynamic("p2.worker-control", "restart", json!({}));
                    flash = Some("Restarting session worker…".into());
                } else {
                    flash = Some("No active session".into());
                }
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay: None,
                    flash,
                };
            }
            "dev-reload" => {
                let _ = view.ensure_hold("p2.plugin-reload", json!({}), "server", json!({}));
                let _ = view.call_dynamic("p2.plugin-reload", "reload", json!({}));
                flash = Some("Reloaded server plugins".into());
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay: None,
                    flash,
                };
            }
            "agents" => {
                let _ = view.ensure_hold("p2.subagents", json!({}), "server", json!({}));
                self.active = Some(ActivePanel::Agents);
            }
            "main" => {
                if let Some((sid, _)) = view.destination().cloned()
                    && let Some(row) = view
                        .catalog_rows()
                        .iter()
                        .find(|r| r["id"].as_str() == Some(sid.as_str()))
                    && let Some(parent) = row["parentId"]
                        .as_str()
                        .or_else(|| row["parent"].as_str())
                        .map(str::to_owned)
                {
                    let _ = view.switch_session(&parent);
                    flash = Some("Switched to parent session".into());
                } else if let Some(first) = view.session_list().first().map(|(id, _, _)| id.clone())
                {
                    let _ = view.switch_session(&first);
                }
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay: None,
                    flash,
                };
            }
            "files" => {
                let dir = args.trim().to_owned();
                if let Some((sid, conv)) = view.destination().cloned() {
                    let _ = view.ensure_hold(
                        "p2.file-index",
                        json!({ "session": sid, "conversation": conv }),
                        "session",
                        json!({ "id": sid }),
                    );
                    let _ = view.call_dynamic("p2.file-index", "list", json!({ "path": dir }));
                }
                self.active = Some(ActivePanel::Files(args.trim().to_owned()));
            }
            "changes" => {
                if let Some((sid, conv)) = view.destination().cloned() {
                    let _ = view.ensure_hold(
                        "p2.workspace",
                        json!({ "session": sid, "conversation": conv }),
                        "session",
                        json!({ "id": sid }),
                    );
                }
                self.active = Some(ActivePanel::Changes);
            }
            "grep" => {
                let query = args.trim().to_owned();
                if query.is_empty() {
                    flash = Some("Usage: /grep <text>".into());
                    return SlashOutcome {
                        handled: true,
                        panel: None,
                        overlay: None,
                        flash,
                    };
                }
                if let Some((sid, conv)) = view.destination().cloned() {
                    let _ = view.ensure_hold(
                        "p2.workspace",
                        json!({ "session": sid, "conversation": conv }),
                        "session",
                        json!({ "id": sid }),
                    );
                    let _ = view.call_dynamic(
                        "p2.workspace",
                        "query",
                        json!({ "kind": "grep", "query": query }),
                    );
                }
                self.active = Some(ActivePanel::Grep(query));
            }
            "discord" => {
                let _ = view.ensure_hold("p2.discord", json!({}), "server", json!({}));
                self.active = Some(ActivePanel::Discord);
            }
            "compact" => {
                let inst = (!args.trim().is_empty()).then(|| args.trim());
                if let Err(err) = view.compact_chat(inst) {
                    flash = Some(line_text(&err.to_string()));
                } else {
                    flash = Some("Compacting context…".into());
                }
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay: None,
                    flash,
                };
            }
            "new-context" => {
                let handoff = (!args.trim().is_empty()).then(|| args.trim());
                if let Err(err) = view.reset_chat(handoff) {
                    flash = Some(line_text(&err.to_string()));
                } else {
                    flash = Some("Started a new context".into());
                }
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay: None,
                    flash,
                };
            }
            "clone" => {
                if let Some((sid, _)) = view.destination().cloned() {
                    if let Err(err) = view.call_session_op("clone", json!({ "session": sid })) {
                        flash = Some(line_text(&err.to_string()));
                    } else {
                        flash = Some("Cloned session".into());
                    }
                }
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay: None,
                    flash,
                };
            }
            "attach" => {
                let target = args.trim();
                flash = Some(if target.is_empty() {
                    "Use @<path> in the editor or Ctrl+V to attach files".into()
                } else {
                    format!("Queued attachment: {target}")
                });
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay: None,
                    flash,
                };
            }
            "detach" => {
                self.close();
                flash = Some("Detached panel".into());
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay: None,
                    flash,
                };
            }
            "copy" => {
                let msg = view.last_message.clone();
                if msg.is_empty() {
                    flash = Some("No assistant message to copy".into());
                } else {
                    push_action(&self.actions, PanelAction::CopyText(msg));
                    flash = Some("Copied last assistant response".into());
                }
                return SlashOutcome {
                    handled: true,
                    panel: None,
                    overlay: None,
                    flash,
                };
            }
            "theme" => {
                self.active = Some(ActivePanel::Themes);
            }
            "prompts" => {
                if let Some((sid, conv)) = view.destination().cloned() {
                    let _ = view.ensure_hold(
                        "p2.prompt-resources",
                        json!({ "session": sid, "conversation": conv }),
                        "session",
                        json!({ "id": sid }),
                    );
                }
                self.active = Some(ActivePanel::Prompts);
            }
            _ => {
                return SlashOutcome {
                    handled: false,
                    panel: None,
                    overlay: None,
                    flash: None,
                };
            }
        }
        SlashOutcome {
            handled: true,
            panel: self.refresh(view, theme),
            overlay,
            flash,
        }
    }

    fn make_model_list_overlay(&self, view: &View, theme: &ThemeRef) -> Box<dyn Component> {
        let default_p = view.default_model["provider"].as_str();
        let default_id = view.default_model["modelId"].as_str();
        let rows: Vec<ListRow> = view
            .models_list
            .iter()
            .map(|m| {
                let provider = m["provider"].as_str().unwrap_or("unknown");
                let id = m["id"]
                    .as_str()
                    .or_else(|| m["modelId"].as_str())
                    .unwrap_or("unknown");
                let is_def = default_p == Some(provider) && default_id == Some(id);
                let marker = if is_def { "● " } else { "" };
                let ctx_str = m["contextWindow"]
                    .as_u64()
                    .map(|w| format!(" ({w} ctx)"))
                    .unwrap_or_default();
                ListRow {
                    key: format!("{provider}\0{id}"),
                    label: format!("{marker}{provider}/{id}"),
                    detail: Some(ctx_str),
                    description: None,
                    color: is_def.then_some(RowColor::Accent),
                }
            })
            .collect();
        let act_sel = self.actions.clone();
        let act_close = self.actions.clone();
        let ctx = SelectorContext::new(
            Box::new(PanelRenderCtx(theme.clone())),
            Box::new(super::facets::PanelKeys),
        );
        let overlay = ListOverlay::new(ctx, rows, Vec::new(), move || {
            push_action(&act_close, PanelAction::CloseOverlay);
        })
        .with_on_select(move |row| {
            if let Some((p, id)) = row.key.split_once('\0') {
                push_action(
                    &act_sel,
                    PanelAction::SetDefaultModel {
                        provider: p.to_owned(),
                        model_id: id.to_owned(),
                    },
                );
                push_action(&act_sel, PanelAction::CloseOverlay);
            }
        });
        Box::new(FramedOverlay::new(
            "Select Model",
            Box::new(overlay),
            theme.clone(),
        ))
    }

    fn make_thinking_list_overlay(&self, view: &View, theme: &ThemeRef) -> Box<dyn Component> {
        let current = view.default_model["thinkingLevel"]
            .as_str()
            .unwrap_or("off");
        let rows: Vec<ListRow> = ["off", "minimal", "low", "medium", "high", "xhigh"]
            .into_iter()
            .map(|lvl| ListRow {
                key: lvl.to_string(),
                label: if lvl == current {
                    format!("● {lvl}")
                } else {
                    lvl.to_string()
                },
                detail: None,
                description: None,
                color: (lvl == current).then_some(RowColor::Accent),
            })
            .collect();
        let act_sel = self.actions.clone();
        let act_close = self.actions.clone();
        let ctx = SelectorContext::new(
            Box::new(PanelRenderCtx(theme.clone())),
            Box::new(super::facets::PanelKeys),
        );
        let overlay = ListOverlay::new(ctx, rows, Vec::new(), move || {
            push_action(&act_close, PanelAction::CloseOverlay);
        })
        .with_on_select(move |row| {
            push_action(
                &act_sel,
                PanelAction::ConfigureChat(json!({ "thinkingLevel": row.key })),
            );
            push_action(&act_sel, PanelAction::CloseOverlay);
        });
        Box::new(FramedOverlay::new(
            "Thinking Level",
            Box::new(overlay),
            theme.clone(),
        ))
    }

    pub(super) fn refresh(&self, view: &View, theme: &ThemeRef) -> Option<PanelFrame> {
        let active = self.active.as_ref()?;
        let (id, title, panel) = match active {
            ActivePanel::Help => ("builtin.help", "Help", build_help(self, theme)),
            ActivePanel::Sessions => (
                "builtin.sessions",
                "Sessions",
                build_sessions(self, view, theme),
            ),
            ActivePanel::Tree => (
                "builtin.tree",
                "Conversation Tree",
                build_tree(self, view, theme),
            ),
            ActivePanel::Models => ("builtin.models", "Models", build_models(self, view, theme)),
            ActivePanel::Cost => (
                "builtin.cost",
                "Session Costs",
                build_cost(self, view, theme),
            ),
            ActivePanel::Status => (
                "builtin.status",
                "Session Status",
                build_status(self, view, theme),
            ),
            ActivePanel::SessionInfo => (
                "builtin.session",
                "Session",
                build_session_info(self, view, theme),
            ),
            ActivePanel::Themes => ("builtin.themes", "Themes", build_themes(self, theme)),
            ActivePanel::Login => (
                "builtin.login",
                "Provider Login",
                build_login(self, view, theme),
            ),
            ActivePanel::Mcp => ("builtin.mcp", "MCP Servers", build_mcp(self, view, theme)),
            ActivePanel::Sharing => (
                "builtin.sharing",
                "Sharing",
                build_sharing(self, view, theme),
            ),
            ActivePanel::Devices => (
                "builtin.devices",
                "Devices",
                build_devices(self, view, theme),
            ),
            ActivePanel::Envs => (
                "builtin.envs",
                "Environments",
                build_envs(self, view, theme),
            ),
            ActivePanel::Ports => (
                "builtin.ports",
                "Forwarded Ports",
                build_ports(self, view, theme),
            ),
            ActivePanel::Artifacts => (
                "builtin.artifacts",
                "Artifacts",
                build_artifacts(self, view, theme),
            ),
            ActivePanel::ChatMode => (
                "builtin.chat-mode",
                "Chat Mode",
                build_chat_mode(self, view, theme),
            ),
            ActivePanel::Logs(src) => (
                "builtin.logs",
                "Server Logs",
                build_logs(self, view, src.as_deref(), theme),
            ),
            ActivePanel::Notices => (
                "builtin.notices",
                "Notices",
                build_notices(self, view, theme),
            ),
            ActivePanel::Downloads => (
                "builtin.downloads",
                "Downloads",
                build_downloads(self, view, theme),
            ),
            ActivePanel::Agents => (
                "builtin.agents",
                "Subagents",
                build_agents(self, view, theme),
            ),
            ActivePanel::Files(dir) => (
                "builtin.files",
                "Workspace Files",
                build_files(self, view, dir, theme),
            ),
            ActivePanel::Changes => (
                "builtin.changes",
                "Workspace Changes",
                build_changes(self, view, theme),
            ),
            ActivePanel::Grep(query) => (
                "builtin.grep",
                "Workspace Search",
                build_grep(self, view, query, theme),
            ),
            ActivePanel::Discord => (
                "builtin.discord",
                "Discord",
                build_discord(self, view, theme),
            ),
            ActivePanel::Prompts => (
                "builtin.prompts",
                "Prompt Templates",
                build_prompts(self, view, theme),
            ),
        };
        let title = title.to_owned();
        Some(PanelFrame::new(
            Box::new(PanelRenderCtx(theme.clone())),
            id.into(),
            Box::new(panel),
            move || title.clone(),
        ))
    }
}

fn build_list_panel(
    panels: &BuiltinPanels,
    header: Vec<String>,
    rows: Vec<InteractiveRow>,
    footer: Vec<String>,
    theme: &ThemeRef,
) -> InteractiveListPanel {
    InteractiveListPanel::new(
        header,
        rows,
        footer,
        panels.selected.clone(),
        panels.actions.clone(),
        theme.clone(),
    )
}

fn plain_rows(lines: Vec<String>) -> Vec<InteractiveRow> {
    lines
        .into_iter()
        .map(|display| InteractiveRow {
            display,
            on_enter: None,
            shortcuts: Vec::new(),
        })
        .collect()
}

fn build_help(panels: &BuiltinPanels, theme_ref: &ThemeRef) -> InteractiveListPanel {
    let theme = theme_ref.current();
    let header = vec![theme.bold(&theme.fg("accent", "Built-in Commands"))];
    let mut rows = Vec::new();
    for (cmd, desc) in [
        ("/new [title]", "Create and switch to a new session"),
        ("/sessions, /resume", "Open the session picker"),
        ("/tree, /fork", "Navigate conversation tree or fork"),
        (
            "/models, /model, /thinking",
            "Configure models and reasoning",
        ),
        (
            "/cost, /status, /session",
            "Costs, worker status & session info",
        ),
        ("/files, /changes, /grep", "Browse files, diffs & search"),
        (
            "/envs, /ports, /mcp, /share",
            "Environments, ports, MCP & sharing",
        ),
        ("/logs, /notices, /artifacts", "Logs, notices & artifacts"),
        (
            "/hotkeys, /settings, /keybindings",
            "Shortcuts, preferences & rebinds",
        ),
        ("/theme [name]", "List or switch TUI color theme"),
        ("/help, /close-panel, /quit", "Help, close panel & exit"),
    ] {
        rows.push(InteractiveRow {
            display: format!("{}  {}", theme.fg("accent", cmd), theme.fg("text", desc)),
            on_enter: None,
            shortcuts: Vec::new(),
        });
    }
    let footer = vec![
        String::new(),
        theme.bold(&theme.fg("accent", "Keybindings")),
        format!("  {}  Command palette", theme.fg("dim", "F1")),
        format!("  {}  Open session picker", theme.fg("dim", "Ctrl+R")),
        format!("  {}  Toggle tool output expand", theme.fg("dim", "Ctrl+O")),
        format!("  {}  Toggle thinking blocks", theme.fg("dim", "Ctrl+T")),
        format!("  {}  External editor", theme.fg("dim", "Ctrl+G")),
        format!("  {}  Toggle side panel focus", theme.fg("dim", "F2")),
        format!(
            "  {}  Clear editor (twice to exit)",
            theme.fg("dim", "Ctrl+C")
        ),
    ];
    build_list_panel(panels, header, rows, footer, theme_ref)
}

fn build_sessions(
    panels: &BuiltinPanels,
    view: &View,
    theme_ref: &ThemeRef,
) -> InteractiveListPanel {
    let theme = theme_ref.current();
    let current = view.destination().map(|(id, _)| id.as_str());
    let mut rows = Vec::new();
    for r in view.catalog_rows() {
        if !r["archivedAt"].is_null() {
            continue;
        }
        let Some(id) = r["id"].as_str() else {
            continue;
        };
        let title = line_text(r["title"].as_str().unwrap_or(id));
        let marker = if current == Some(id) {
            theme.fg("accent", "● ")
        } else {
            "  ".to_string()
        };
        let cwd = r["cwd"].as_str().unwrap_or("");
        rows.push(InteractiveRow {
            display: format!("{marker}{title} {}", theme.fg("dim", cwd)),
            on_enter: Some(PanelAction::SwitchSession(id.to_owned())),
            shortcuts: vec![(
                "ctrl+a".to_owned(),
                PanelAction::ArchiveSession(id.to_owned()),
            )],
        });
    }
    if rows.is_empty() {
        rows = plain_rows(vec![theme.fg("muted", "No active sessions.")]);
    }
    build_list_panel(
        panels,
        vec![theme.bold(&theme.fg("accent", "Sessions"))],
        rows,
        vec![theme.fg("dim", "j/k select · Enter switch · Ctrl+A archive")],
        theme_ref,
    )
}

fn build_models(panels: &BuiltinPanels, view: &View, theme_ref: &ThemeRef) -> InteractiveListPanel {
    let theme = theme_ref.current();
    if let Some(err) = &view.models_error {
        return build_list_panel(
            panels,
            vec![],
            plain_rows(vec![theme.fg("error", err)]),
            vec![],
            theme_ref,
        );
    }
    if !view.models_ready {
        return build_list_panel(
            panels,
            vec![],
            plain_rows(vec![theme.fg("muted", "Loading models…")]),
            vec![],
            theme_ref,
        );
    }
    let mut header = Vec::new();
    let default_provider = view.default_model["provider"].as_str();
    let default_id = view.default_model["modelId"].as_str();
    if let (Some(provider), Some(id)) = (default_provider, default_id) {
        let thinking = view.default_model["thinkingLevel"]
            .as_str()
            .map(|t| format!(" ({t})"))
            .unwrap_or_default();
        header.push(format!(
            "{} {}{}",
            theme.fg("muted", "Default:"),
            theme.fg("accent", &format!("{provider}/{id}")),
            theme.fg("dim", &thinking)
        ));
        header.push(String::new());
    }
    header.push(theme.bold(&theme.fg(
        "text",
        &format!("Configured Models ({})", view.models_list.len()),
    )));
    let mut rows = Vec::new();
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
        rows.push(InteractiveRow {
            display: format!("{marker}{styled}{ctx}"),
            on_enter: Some(PanelAction::SetDefaultModel {
                provider: provider.to_owned(),
                model_id: id.to_owned(),
            }),
            shortcuts: Vec::new(),
        });
    }
    if rows.is_empty() {
        rows = plain_rows(vec![theme.fg("muted", "  No models configured.")]);
    }
    build_list_panel(
        panels,
        header,
        rows,
        vec![theme.fg(
            "dim",
            "Enter set default · /model, /thinking, /scoped-models",
        )],
        theme_ref,
    )
}

fn build_tree(panels: &BuiltinPanels, view: &View, theme_ref: &ThemeRef) -> InteractiveListPanel {
    let theme = theme_ref.current();
    if let Some(err) = &view.tree_error {
        return build_list_panel(
            panels,
            vec![],
            plain_rows(vec![theme.fg("error", err)]),
            vec![],
            theme_ref,
        );
    }
    let Some(tree) = &view.tree_data else {
        return build_list_panel(
            panels,
            vec![],
            plain_rows(vec![theme.fg("muted", "Loading conversation tree…")]),
            vec![],
            theme_ref,
        );
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
    let mut header = vec![format!("{} #{}", theme.fg("muted", "Main branch:"), main)];
    if let Some(label) = labels.get(&main) {
        header.push(format!("{} {}", theme.fg("muted", "Label:"), label));
    }
    header.push(String::new());

    let nodes: Vec<&Value> = tree["nodes"].as_array().into_iter().flatten().collect();
    if nodes.is_empty() {
        return build_list_panel(
            panels,
            header,
            plain_rows(vec![theme.fg("muted", "No messages in this session yet.")]),
            vec![],
            theme_ref,
        );
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

    let mut rows = Vec::new();
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
        rows.push(InteractiveRow {
            display: format!(
                "{}{}{role_tag}{preview}",
                theme.fg("dim", &prefix),
                active_dot
            ),
            on_enter: Some(PanelAction::TreeNavigate(entry)),
            shortcuts: Vec::new(),
        });
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
    build_list_panel(
        panels,
        header,
        rows,
        vec![theme.fg("dim", "j/k select · Enter jump to branch")],
        theme_ref,
    )
}

fn build_cost(panels: &BuiltinPanels, view: &View, theme_ref: &ThemeRef) -> InteractiveListPanel {
    let theme = theme_ref.current();
    if let Some(err) = &view.costs_error {
        return build_list_panel(
            panels,
            vec![],
            plain_rows(vec![theme.fg("error", err)]),
            vec![],
            theme_ref,
        );
    }
    let mut header = Vec::new();
    if let Some(stats) = &view.stats_data {
        let tokens = &stats["tokens"];
        if !tokens.is_null() {
            let input = tokens["input"].as_u64().unwrap_or(0);
            let output = tokens["output"].as_u64().unwrap_or(0);
            let cache_read = tokens["cacheRead"].as_u64().unwrap_or(0);
            let cache_write = tokens["cacheWrite"].as_u64().unwrap_or(0);
            header.push(theme.bold(&theme.fg("text", "Token Totals")));
            header.push(format!(
                "  in: {input}  out: {output}  cacheR: {cache_read}  cacheW: {cache_write}"
            ));
            if let Some(total_cost) = stats["cost"].as_f64() {
                header.push(format!(
                    "  {} ${:.4}",
                    theme.fg("muted", "Total cost:"),
                    total_cost
                ));
            }
            header.push(String::new());
        }
    }
    let Some(costs) = &view.costs_data else {
        let fallback = if header.is_empty() {
            vec![theme.fg("muted", "Loading session costs…")]
        } else {
            Vec::new()
        };
        return build_list_panel(panels, header, plain_rows(fallback), vec![], theme_ref);
    };
    let rows_val = costs.as_array().map(Vec::as_slice).unwrap_or(&[]);
    if rows_val.is_empty() {
        return build_list_panel(
            panels,
            header,
            plain_rows(vec![theme.fg("muted", "No model responses recorded yet.")]),
            vec![],
            theme_ref,
        );
    }
    let total: f64 = rows_val.iter().filter_map(|r| r["cost"].as_f64()).sum();
    header.push(theme.bold(&theme.fg(
        "accent",
        &format!("Responses ({})  Total: ${total:.4}", rows_val.len()),
    )));
    let rows = rows_val
        .iter()
        .map(|row| {
            let id = row["entryId"].as_str().unwrap_or("?");
            let provider = row["provider"].as_str().unwrap_or("?");
            let model = row["model"].as_str().unwrap_or("?");
            let cost = row["cost"].as_f64().unwrap_or(0.0);
            let cache = row["cacheHit"]
                .as_f64()
                .map(|c| format!("  cache {:.0}%", c * 100.0))
                .unwrap_or_default();
            InteractiveRow {
                display: format!(
                    "  #{id} {provider}/{model}  ${cost:.4}{}",
                    theme.fg("dim", &cache)
                ),
                on_enter: None,
                shortcuts: Vec::new(),
            }
        })
        .collect();
    build_list_panel(panels, header, rows, vec![], theme_ref)
}

fn build_status(panels: &BuiltinPanels, view: &View, theme_ref: &ThemeRef) -> InteractiveListPanel {
    let theme = theme_ref.current();
    if let Some(err) = &view.status_error {
        return build_list_panel(
            panels,
            vec![],
            plain_rows(vec![theme.fg("error", err)]),
            vec![],
            theme_ref,
        );
    }
    let mut header = Vec::new();
    if let Some((id, conv)) = view.destination() {
        header.push(format!(
            "{} {} (#{conv})",
            theme.fg("muted", "Current:"),
            theme.fg("accent", view.title())
        ));
        header.push(theme.fg("dim", &format!("  session: {id}")));
        header.push(String::new());
    }
    let Some(map) = view.statuses.as_ref().and_then(|s| s.as_object()) else {
        let msg = if view.statuses.is_none() {
            "Loading session status…"
        } else {
            "No active workers."
        };
        return build_list_panel(
            panels,
            header,
            plain_rows(vec![theme.fg("muted", msg)]),
            vec![],
            theme_ref,
        );
    };
    let current_id = view.destination().map(|(id, _)| id.as_str());
    header.push(theme.bold(&theme.fg("text", &format!("Workers ({})", map.len()))));
    let mut rows = Vec::new();
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
        rows.push(InteractiveRow {
            display: format!("{marker}{session_id}: {state} (tasks: {tasks})"),
            on_enter: Some(PanelAction::SwitchSession(session_id.clone())),
            shortcuts: vec![(
                "r".into(),
                PanelAction::CallContract {
                    contract: "p2.worker-control".into(),
                    method: "restart".into(),
                    args: json!({}),
                },
            )],
        });
    }
    build_list_panel(
        panels,
        header,
        rows,
        vec![theme.fg("dim", "/retry-worker · /dev-reload · /dev-restart")],
        theme_ref,
    )
}

fn build_session_info(
    panels: &BuiltinPanels,
    view: &View,
    theme_ref: &ThemeRef,
) -> InteractiveListPanel {
    let theme = theme_ref.current();
    let mut lines = Vec::new();
    if let Some((sid, conv)) = view.destination() {
        lines.push(format!(
            "{} {}",
            theme.fg("muted", "Session:"),
            theme.fg("accent", sid)
        ));
        lines.push(format!("{} {}", theme.fg("muted", "Title:"), view.title()));
        lines.push(format!("{} #{conv}", theme.fg("muted", "Conversation:")));
        lines.push(format!(
            "{} {}",
            theme.fg("muted", "CWD:"),
            view.active_cwd()
        ));
        if let Some(env_name) = view.active_env() {
            lines.push(format!("{} {env_name}", theme.fg("muted", "Env:")));
        }
        if let Some(model) = &view.chat_model
            && let (Some(p), Some(id)) = (model["provider"].as_str(), model["modelId"].as_str())
        {
            lines.push(format!("{} {p}/{id}", theme.fg("muted", "Model:")));
        }
    } else {
        lines.push(theme.fg("muted", "No active session"));
    }
    build_list_panel(panels, vec![], plain_rows(lines), vec![], theme_ref)
}

fn build_themes(panels: &BuiltinPanels, theme_ref: &ThemeRef) -> InteractiveListPanel {
    let theme = theme_ref.current();
    let header = vec![
        theme.bold(&theme.fg("accent", "Available Themes")),
        String::new(),
    ];
    let rows = panels
        .available_themes
        .iter()
        .map(|name| {
            let is_current = name == &panels.current_theme;
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
            InteractiveRow {
                display: format!("{marker}{label}"),
                on_enter: Some(PanelAction::SwitchTheme(name.clone())),
                shortcuts: Vec::new(),
            }
        })
        .collect();
    build_list_panel(
        panels,
        header,
        rows,
        vec![
            String::new(),
            theme.fg("dim", "Enter to switch or /theme <name>"),
        ],
        theme_ref,
    )
}

fn contract_error_or_loading(
    panels: &BuiltinPanels,
    view: &View,
    contract: &str,
    theme_ref: &ThemeRef,
) -> Option<InteractiveListPanel> {
    let theme = theme_ref.current();
    if let Some(err) = view.dynamic_error(contract) {
        return Some(build_list_panel(
            panels,
            vec![],
            plain_rows(vec![theme.fg("error", err)]),
            vec![],
            theme_ref,
        ));
    }
    None
}

fn build_login(panels: &BuiltinPanels, view: &View, theme_ref: &ThemeRef) -> InteractiveListPanel {
    if let Some(p) = contract_error_or_loading(panels, view, "p2.provider-login", theme_ref) {
        return p;
    }
    let theme = theme_ref.current();
    let mut header = vec![theme.bold(&theme.fg("accent", "Provider Login"))];
    let mut rows = Vec::new();
    if let Some(login) = view.dynamic_state("p2.provider-login", "login")
        && !login.is_null()
    {
        if let Some(url) = login["url"].as_str() {
            header.push(format!("URL: {url}"));
            rows.push(InteractiveRow {
                display: theme.fg("accent", "Open login URL in browser"),
                on_enter: Some(PanelAction::OpenBrowser(url.to_owned())),
                shortcuts: vec![],
            });
        }
        if let Some(code) = login["code"].as_str() {
            header.push(format!("Code: {code}"));
        }
        if let Some(msg) = login["message"].as_str() {
            header.push(line_text(msg));
        }
    }
    for p in view
        .dynamic_state("p2.provider-login", "providers")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let id = p["id"].as_str().unwrap_or("?");
        let logged = p["loggedIn"].as_bool().unwrap_or(false);
        let mark = if logged {
            theme.fg("accent", "● ")
        } else {
            "  ".to_string()
        };
        rows.push(InteractiveRow {
            display: format!("{mark}{id} {}", if logged { "(signed in)" } else { "" }),
            on_enter: Some(PanelAction::CallContract {
                contract: "p2.provider-login".into(),
                method: "start".into(),
                args: json!({ "provider": id }),
            }),
            shortcuts: vec![(
                "x".into(),
                PanelAction::CallContract {
                    contract: "p2.provider-login".into(),
                    method: "logout".into(),
                    args: json!({ "provider": id }),
                },
            )],
        });
    }
    if rows.is_empty() {
        rows = plain_rows(vec![theme.fg("muted", "Loading providers…")]);
    }
    build_list_panel(
        panels,
        header,
        rows,
        vec![theme.fg("dim", "Enter login · x logout · /login <provider>")],
        theme_ref,
    )
}

fn build_mcp(panels: &BuiltinPanels, view: &View, theme_ref: &ThemeRef) -> InteractiveListPanel {
    if let Some(p) = contract_error_or_loading(panels, view, "p2.mcp", theme_ref) {
        return p;
    }
    let theme = theme_ref.current();
    let mut rows = Vec::new();
    for item in view
        .dynamic_state("p2.mcp", "instances")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let name = item["name"]
            .as_str()
            .or_else(|| item["id"].as_str())
            .unwrap_or("mcp");
        let status = item["status"].as_str().unwrap_or("unknown");
        rows.push(InteractiveRow {
            display: format!("{name}  {}", theme.fg("dim", status)),
            on_enter: Some(PanelAction::CallContract {
                contract: "p2.mcp".into(),
                method: "restart".into(),
                args: json!({ "name": name }),
            }),
            shortcuts: Vec::new(),
        });
    }
    if rows.is_empty() {
        rows = plain_rows(vec![theme.fg("muted", "No MCP servers configured.")]);
    }
    build_list_panel(
        panels,
        vec![theme.bold(&theme.fg("accent", "MCP Servers"))],
        rows,
        vec![theme.fg("dim", "Enter restart · /mcp paste <url>")],
        theme_ref,
    )
}

fn build_sharing(
    panels: &BuiltinPanels,
    view: &View,
    theme_ref: &ThemeRef,
) -> InteractiveListPanel {
    if let Some(p) = contract_error_or_loading(panels, view, "p2.sharing", theme_ref) {
        return p;
    }
    let theme = theme_ref.current();
    let mut rows = Vec::new();
    for link in view
        .dynamic_state("p2.sharing", "links")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let id = link["id"].as_str().unwrap_or("?");
        let url = link["url"].as_str().unwrap_or(id);
        rows.push(InteractiveRow {
            display: format!("link: {url}"),
            on_enter: Some(PanelAction::CopyText(url.to_owned())),
            shortcuts: vec![(
                "x".into(),
                PanelAction::CallContract {
                    contract: "p2.sharing".into(),
                    method: "remove".into(),
                    args: json!({ "id": id }),
                },
            )],
        });
    }
    for guest in view
        .dynamic_state("p2.sharing", "guests")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let name = guest["name"].as_str().unwrap_or("guest");
        rows.push(InteractiveRow {
            display: format!("guest: {name}"),
            on_enter: None,
            shortcuts: Vec::new(),
        });
    }
    if rows.is_empty() {
        rows = plain_rows(vec![theme.fg("muted", "No active share links or guests.")]);
    }
    build_list_panel(
        panels,
        vec![theme.bold(&theme.fg("accent", "Sharing"))],
        rows,
        vec![theme.fg("dim", "/share [email [full]] · Enter copy link · x revoke")],
        theme_ref,
    )
}

fn build_devices(
    panels: &BuiltinPanels,
    view: &View,
    theme_ref: &ThemeRef,
) -> InteractiveListPanel {
    if let Some(p) = contract_error_or_loading(panels, view, "p2.devices", theme_ref) {
        return p;
    }
    let theme = theme_ref.current();
    let can_mint = view
        .dynamic_state("p2.devices", "canMint")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let rows = vec![InteractiveRow {
        display: if can_mint {
            theme.fg("accent", "Mint new pairing token (press Enter)")
        } else {
            theme.fg("muted", "Device pairing unavailable on this host")
        },
        on_enter: can_mint.then(|| PanelAction::CallContract {
            contract: "p2.devices".into(),
            method: "token".into(),
            args: json!({}),
        }),
        shortcuts: Vec::new(),
    }];
    build_list_panel(
        panels,
        vec![theme.bold(&theme.fg("accent", "Paired Devices"))],
        rows,
        vec![],
        theme_ref,
    )
}

fn build_envs(panels: &BuiltinPanels, view: &View, theme_ref: &ThemeRef) -> InteractiveListPanel {
    if let Some(p) = contract_error_or_loading(panels, view, "p2.envs", theme_ref) {
        return p;
    }
    let theme = theme_ref.current();
    let current_env = view.active_env();
    let sid = view
        .destination()
        .map(|(s, _)| s.clone())
        .unwrap_or_default();
    let cwd = view.active_cwd();
    let mut rows = Vec::new();
    for env in view
        .dynamic_state("p2.envs", "envs")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let name = env["name"].as_str().unwrap_or("?");
        let status = env["status"].as_str().unwrap_or("");
        let is_cur = current_env.as_deref() == Some(name);
        let mark = if is_cur {
            theme.fg("accent", "● ")
        } else {
            "  ".into()
        };
        rows.push(InteractiveRow {
            display: format!("{mark}{name} {}", theme.fg("dim", status)),
            on_enter: (!sid.is_empty()).then(|| PanelAction::RelocateSession {
                session: sid.clone(),
                env: Some(name.to_owned()),
                cwd: cwd.clone(),
            }),
            shortcuts: vec![
                (
                    "ctrl+t".into(),
                    PanelAction::CallContract {
                        contract: "p2.envs".into(),
                        method: "test".into(),
                        args: json!({ "name": name }),
                    },
                ),
                (
                    "ctrl+k".into(),
                    PanelAction::CallContract {
                        contract: "p2.envs".into(),
                        method: "acceptHostKey".into(),
                        args: json!({ "name": name }),
                    },
                ),
                (
                    "ctrl+f".into(),
                    PanelAction::CallContract {
                        contract: "p2.envs".into(),
                        method: "forgetHostKey".into(),
                        args: json!({ "name": name }),
                    },
                ),
            ],
        });
    }
    if rows.is_empty() {
        rows = plain_rows(vec![
            theme.fg("muted", "No remote environments configured."),
        ]);
    }
    build_list_panel(
        panels,
        vec![theme.bold(&theme.fg("accent", "Environments"))],
        rows,
        vec![theme.fg(
            "dim",
            "Enter move · Ctrl+T test · Ctrl+K accept key · Ctrl+F forget",
        )],
        theme_ref,
    )
}

fn build_ports(panels: &BuiltinPanels, view: &View, theme_ref: &ThemeRef) -> InteractiveListPanel {
    if let Some(p) = contract_error_or_loading(panels, view, "p2.ports", theme_ref) {
        return p;
    }
    let theme = theme_ref.current();
    let mut rows = Vec::new();
    for fwd in view
        .dynamic_state("p2.ports", "forwards")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let port = fwd["port"].as_u64().unwrap_or(0);
        let name = fwd["name"].as_str().unwrap_or("");
        let public = fwd["public"].as_bool().unwrap_or(false);
        let id = fwd["id"].as_str().unwrap_or("");
        let mut shortcuts = vec![(
            "x".into(),
            PanelAction::CallContract {
                contract: "p2.ports".into(),
                method: "remove".into(),
                args: json!({ "id": id }),
            },
        )];
        if let Some(url) = fwd["url"].as_str() {
            shortcuts.push(("o".into(), PanelAction::OpenBrowser(url.to_owned())));
        }
        rows.push(InteractiveRow {
            display: format!(
                ":{port} {name}  {}",
                theme.fg("dim", if public { "public" } else { "private" })
            ),
            on_enter: Some(PanelAction::CallContract {
                contract: "p2.ports".into(),
                method: "setPublic".into(),
                args: json!({ "id": id, "public": !public }),
            }),
            shortcuts,
        });
    }
    if rows.is_empty() {
        rows = plain_rows(vec![theme.fg("muted", "No forwarded ports.")]);
    }
    build_list_panel(
        panels,
        vec![theme.bold(&theme.fg("accent", "Forwarded Ports"))],
        rows,
        vec![theme.fg("dim", "/ports open <port> · Enter toggle public · x remove")],
        theme_ref,
    )
}

fn build_artifacts(
    panels: &BuiltinPanels,
    view: &View,
    theme_ref: &ThemeRef,
) -> InteractiveListPanel {
    if let Some(p) = contract_error_or_loading(panels, view, "p2.artifacts", theme_ref) {
        return p;
    }
    let theme = theme_ref.current();
    let mut rows = Vec::new();
    for art in view
        .dynamic_state("p2.artifacts", "artifacts")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let id = art["id"]
            .as_str()
            .or_else(|| art["name"].as_str())
            .unwrap_or("?");
        let title = art["title"].as_str().unwrap_or(id);
        rows.push(InteractiveRow {
            display: format!("{id}  {}", theme.fg("muted", title)),
            on_enter: Some(PanelAction::CallContract {
                contract: "p2.artifacts".into(),
                method: "read".into(),
                args: json!({ "id": id }),
            }),
            shortcuts: vec![(
                "x".into(),
                PanelAction::CallContract {
                    contract: "p2.artifacts".into(),
                    method: "remove".into(),
                    args: json!({ "id": id }),
                },
            )],
        });
    }
    if rows.is_empty() {
        rows = plain_rows(vec![theme.fg("muted", "No artifacts in this session.")]);
    }
    build_list_panel(
        panels,
        vec![theme.bold(&theme.fg("accent", "Artifacts"))],
        rows,
        vec![theme.fg("dim", "Enter read · x remove")],
        theme_ref,
    )
}

fn build_chat_mode(
    panels: &BuiltinPanels,
    view: &View,
    theme_ref: &ThemeRef,
) -> InteractiveListPanel {
    if let Some(p) = contract_error_or_loading(panels, view, "p2.chat-mode", theme_ref) {
        return p;
    }
    let theme = theme_ref.current();
    let enabled = view
        .dynamic_state("p2.chat-mode", "enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let name = view
        .dynamic_state("p2.chat-mode", "name")
        .and_then(|v| v.as_str())
        .unwrap_or("Pi");
    let rows = vec![InteractiveRow {
        display: format!(
            "Chat mode: {}  (agent: {name})",
            if enabled {
                theme.fg("accent", "enabled")
            } else {
                theme.fg("muted", "disabled")
            }
        ),
        on_enter: Some(PanelAction::CallContract {
            contract: "p2.chat-mode".into(),
            method: "set".into(),
            args: json!({ "enabled": !enabled }),
        }),
        shortcuts: Vec::new(),
    }];
    build_list_panel(
        panels,
        vec![theme.bold(&theme.fg("accent", "Chat Mode"))],
        rows,
        vec![theme.fg(
            "dim",
            "Enter toggle · /chat-mode [on|off|<name>] · /reply <msg>",
        )],
        theme_ref,
    )
}

fn build_logs(
    panels: &BuiltinPanels,
    view: &View,
    source: Option<&str>,
    theme_ref: &ThemeRef,
) -> InteractiveListPanel {
    if let Some(p) = contract_error_or_loading(panels, view, "p2.logs", theme_ref) {
        return p;
    }
    let theme = theme_ref.current();
    let mut header = vec![theme.bold(&theme.fg(
        "accent",
        &format!(
            "Logs{}",
            source.map(|s| format!(" ({s})")).unwrap_or_default()
        ),
    ))];
    if let Some(sources) = view
        .dynamic_state("p2.logs", "sources")
        .and_then(|v| v.as_array())
    {
        let list: Vec<&str> = sources.iter().filter_map(|v| v.as_str()).collect();
        if !list.is_empty() {
            header.push(theme.fg("dim", &format!("Sources: {}", list.join(", "))));
        }
    }
    let mut rows = Vec::new();
    for line in view
        .dynamic_state("p2.logs", "lines")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let msg = line["text"]
            .as_str()
            .or_else(|| line["message"].as_str())
            .or_else(|| line.as_str())
            .map(line_text)
            .unwrap_or_else(|| line.to_string());
        rows.push(InteractiveRow {
            display: msg,
            on_enter: None,
            shortcuts: Vec::new(),
        });
    }
    if rows.is_empty() {
        rows = plain_rows(vec![theme.fg("muted", "No log lines yet.")]);
    }
    build_list_panel(panels, header, rows, vec![], theme_ref)
}

fn build_notices(
    panels: &BuiltinPanels,
    view: &View,
    theme_ref: &ThemeRef,
) -> InteractiveListPanel {
    if let Some(p) = contract_error_or_loading(panels, view, "p2.log-notices", theme_ref) {
        return p;
    }
    let theme = theme_ref.current();
    let mut rows = Vec::new();
    for n in view
        .dynamic_state("p2.log-notices", "notices")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let id = n["id"].as_str().unwrap_or("");
        let msg = n["message"].as_str().unwrap_or("notice");
        rows.push(InteractiveRow {
            display: line_text(msg),
            on_enter: Some(PanelAction::CallContract {
                contract: "p2.log-notices".into(),
                method: "dismiss".into(),
                args: json!({ "id": id }),
            }),
            shortcuts: Vec::new(),
        });
    }
    if rows.is_empty() {
        rows = plain_rows(vec![theme.fg("muted", "No notices.")]);
    }
    build_list_panel(
        panels,
        vec![theme.bold(&theme.fg("accent", "Notices"))],
        rows,
        vec![theme.fg("dim", "Enter dismiss")],
        theme_ref,
    )
}

fn build_downloads(
    panels: &BuiltinPanels,
    view: &View,
    theme_ref: &ThemeRef,
) -> InteractiveListPanel {
    let theme = theme_ref.current();
    let rows = match view
        .dynamic_results
        .get(&("p2.sent-files".into(), "preview".into()))
    {
        Some(Ok(val)) => val
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| InteractiveRow {
                display: line_text(f["name"].as_str().unwrap_or(&f.to_string())),
                on_enter: None,
                shortcuts: Vec::new(),
            })
            .collect(),
        _ => plain_rows(vec![
            theme.fg("muted", "No sent files / downloads recorded yet."),
        ]),
    };
    build_list_panel(
        panels,
        vec![theme.bold(&theme.fg("accent", "Downloads"))],
        rows,
        vec![],
        theme_ref,
    )
}

fn build_agents(panels: &BuiltinPanels, view: &View, theme_ref: &ThemeRef) -> InteractiveListPanel {
    if let Some(p) = contract_error_or_loading(panels, view, "p2.subagents", theme_ref) {
        return p;
    }
    let theme = theme_ref.current();
    let mut rows = Vec::new();
    for child in view
        .dynamic_state("p2.subagents", "children")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let id = child["session"]
            .as_str()
            .or_else(|| child["id"].as_str())
            .unwrap_or("");
        let title = child["title"]
            .as_str()
            .or_else(|| child["role"].as_str())
            .unwrap_or(id);
        rows.push(InteractiveRow {
            display: format!("{} {}", theme.fg("accent", title), theme.fg("dim", id)),
            on_enter: (!id.is_empty()).then(|| PanelAction::SwitchSession(id.to_owned())),
            shortcuts: Vec::new(),
        });
    }
    if rows.is_empty() {
        rows = plain_rows(vec![theme.fg("muted", "No active subagents.")]);
    }
    build_list_panel(
        panels,
        vec![theme.bold(&theme.fg("accent", "Subagents"))],
        rows,
        vec![theme.fg("dim", "Enter open subagent · /main returns to parent")],
        theme_ref,
    )
}

fn build_files(
    panels: &BuiltinPanels,
    view: &View,
    dir: &str,
    theme_ref: &ThemeRef,
) -> InteractiveListPanel {
    let theme = theme_ref.current();
    let header = vec![theme.bold(&theme.fg(
        "accent",
        &format!(
            "Files{}",
            if dir.is_empty() {
                String::new()
            } else {
                format!(": {dir}")
            }
        ),
    ))];
    let mut rows = Vec::new();
    if let Some(Ok(val)) = view
        .dynamic_results
        .get(&("p2.file-index".into(), "list".into()))
    {
        for item in val["entries"]
            .as_array()
            .or_else(|| val.as_array())
            .into_iter()
            .flatten()
        {
            let name = item["name"]
                .as_str()
                .or_else(|| item.as_str())
                .unwrap_or("?");
            let is_dir = item["dir"].as_bool().unwrap_or(false);
            let full = if dir.is_empty() {
                name.to_owned()
            } else {
                format!("{}/{}", dir.trim_end_matches('/'), name)
            };
            rows.push(InteractiveRow {
                display: format!("{}{}", name, if is_dir { "/" } else { "" }),
                on_enter: Some(PanelAction::CallContract {
                    contract: "p2.workspace".into(),
                    method: "query".into(),
                    args: json!({ "kind": "read", "path": full }),
                }),
                shortcuts: Vec::new(),
            });
        }
    }
    if rows.is_empty() {
        rows = plain_rows(vec![theme.fg("muted", "Loading workspace files…")]);
    }
    build_list_panel(
        panels,
        header,
        rows,
        vec![theme.fg("dim", "Enter open file · /changes diffs · /grep <text>")],
        theme_ref,
    )
}

fn build_changes(
    panels: &BuiltinPanels,
    view: &View,
    theme_ref: &ThemeRef,
) -> InteractiveListPanel {
    if let Some(p) = contract_error_or_loading(panels, view, "p2.workspace", theme_ref) {
        return p;
    }
    let theme = theme_ref.current();
    let mut rows = Vec::new();
    for ch in view
        .dynamic_state("p2.workspace", "changes")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let path = ch["path"].as_str().unwrap_or("?");
        let status = ch["status"].as_str().unwrap_or("M");
        let diff = ch["diff"].as_str().unwrap_or("").to_owned();
        rows.push(InteractiveRow {
            display: format!("{} {path}", theme.fg("accent", status)),
            on_enter: Some(if diff.is_empty() {
                PanelAction::CallContract {
                    contract: "p2.workspace".into(),
                    method: "query".into(),
                    args: json!({ "kind": "diff", "path": path }),
                }
            } else {
                PanelAction::OpenDiffOverlay {
                    title: format!("Diff: {path}"),
                    path: path.to_owned(),
                    diff,
                }
            }),
            shortcuts: Vec::new(),
        });
    }
    if rows.is_empty() {
        rows = plain_rows(vec![theme.fg("muted", "Working tree clean.")]);
    }
    build_list_panel(
        panels,
        vec![theme.bold(&theme.fg("accent", "Workspace Changes"))],
        rows,
        vec![theme.fg("dim", "Enter view diff")],
        theme_ref,
    )
}

fn build_grep(
    panels: &BuiltinPanels,
    view: &View,
    query: &str,
    theme_ref: &ThemeRef,
) -> InteractiveListPanel {
    let theme = theme_ref.current();
    let header = vec![theme.bold(&theme.fg("accent", &format!("Grep: {query}")))];
    let mut rows = Vec::new();
    if let Some(Ok(val)) = view
        .dynamic_results
        .get(&("p2.workspace".into(), "query".into()))
    {
        for hit in val["matches"]
            .as_array()
            .or_else(|| val.as_array())
            .into_iter()
            .flatten()
        {
            let path = hit["path"].as_str().unwrap_or("?");
            let line = hit["line"].as_u64().unwrap_or(0);
            let text = line_text(hit["text"].as_str().unwrap_or(""));
            rows.push(InteractiveRow {
                display: format!("{path}:{line}  {text}"),
                on_enter: Some(PanelAction::OpenTextOverlay {
                    title: format!("{path}:{line}"),
                    lines: vec![text],
                }),
                shortcuts: Vec::new(),
            });
        }
    }
    if rows.is_empty() {
        rows = plain_rows(vec![theme.fg("muted", "Searching… (or no matches)")]);
    }
    build_list_panel(panels, header, rows, vec![], theme_ref)
}

fn build_discord(
    panels: &BuiltinPanels,
    view: &View,
    theme_ref: &ThemeRef,
) -> InteractiveListPanel {
    if let Some(p) = contract_error_or_loading(panels, view, "p2.discord", theme_ref) {
        return p;
    }
    let theme = theme_ref.current();
    let mut rows = Vec::new();
    for bot in view
        .dynamic_state("p2.discord", "bots")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let id = bot["id"].as_str().unwrap_or("bot");
        let state = bot["status"]["state"].as_str().unwrap_or("unknown");
        rows.push(InteractiveRow {
            display: format!("bot: {id} ({state})"),
            on_enter: None,
            shortcuts: Vec::new(),
        });
    }
    for b in view
        .dynamic_state("p2.discord", "bindings")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let ch = b["channelName"]
            .as_str()
            .or_else(|| b["channelId"].as_str())
            .unwrap_or("?");
        let sid = b["session"].as_str().unwrap_or("?");
        let cid = b["channelId"].as_str().unwrap_or("");
        rows.push(InteractiveRow {
            display: format!("#{ch} -> {sid}"),
            on_enter: Some(PanelAction::SwitchSession(sid.to_owned())),
            shortcuts: vec![(
                "x".into(),
                PanelAction::CallContract {
                    contract: "p2.discord".into(),
                    method: "unlink".into(),
                    args: json!({ "channelId": cid }),
                },
            )],
        });
    }
    if rows.is_empty() {
        rows = plain_rows(vec![
            theme.fg("muted", "No Discord bots or channel bindings."),
        ]);
    }
    build_list_panel(
        panels,
        vec![theme.bold(&theme.fg("accent", "Discord"))],
        rows,
        vec![theme.fg("dim", "Enter open linked session · x unlink")],
        theme_ref,
    )
}

fn build_prompts(
    panels: &BuiltinPanels,
    view: &View,
    theme_ref: &ThemeRef,
) -> InteractiveListPanel {
    if let Some(p) = contract_error_or_loading(panels, view, "p2.prompt-resources", theme_ref) {
        return p;
    }
    let theme = theme_ref.current();
    let mut rows = Vec::new();
    for tpl in view
        .dynamic_state("p2.prompt-resources", "templates")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let name = tpl["name"].as_str().unwrap_or("?");
        let desc = tpl["description"].as_str().unwrap_or("");
        rows.push(InteractiveRow {
            display: format!("/{name}  {}", theme.fg("muted", desc)),
            on_enter: Some(PanelAction::CallContract {
                contract: "p2.prompt-resources".into(),
                method: "expand".into(),
                args: json!({ "name": name }),
            }),
            shortcuts: Vec::new(),
        });
    }
    for skill in view
        .dynamic_state("p2.prompt-resources", "skills")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let name = skill["name"].as_str().unwrap_or("?");
        rows.push(InteractiveRow {
            display: format!("skill: {name}"),
            on_enter: None,
            shortcuts: Vec::new(),
        });
    }
    if rows.is_empty() {
        rows = plain_rows(vec![theme.fg("muted", "No prompt templates or skills.")]);
    }
    build_list_panel(
        panels,
        vec![theme.bold(&theme.fg("accent", "Prompt Templates"))],
        rows,
        vec![],
        theme_ref,
    )
}

fn parse_env_path(input: &str) -> Option<(Option<String>, String)> {
    let (env, rest) = input.split_once(':')?;
    if env.is_empty() || env.contains('/') || env.contains(' ') {
        return None;
    }
    let env_opt = if env == "local" {
        None
    } else {
        Some(env.to_owned())
    };
    Some((env_opt, rest.to_owned()))
}

fn is_path_like(s: &str) -> bool {
    s.starts_with('/') || s.starts_with("~/") || s == "." || s == ".." || s.starts_with("./")
}

fn resolve_relative_path(base: &str, target: &str) -> String {
    if target.starts_with('/') || target.starts_with("~/") || base.is_empty() {
        target.to_owned()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), target)
    }
}

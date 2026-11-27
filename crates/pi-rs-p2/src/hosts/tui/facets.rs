//! Interactive facet helpers and `PanelAction` dispatch queue shared across the ~20 built-in plugin facets.

use super::view::View;
use pi_rs_p2_tui::{
    ThemeRef,
    components::{
        PanelContent, RenderDiffOptions, ScopedModelsSelectorComponent, SelectorContext,
        SelectorKeys, SelectorModel, TreeRole, TreeSelectorComponent, TreeSelectorNode,
        UserMessageItem, UserMessageSelectorComponent, render_diff,
    },
    line_text,
};
use pi_rs_tui::{Component, Focusable, truncate_to_width};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

#[derive(Debug, Clone)]
pub(super) enum PanelAction {
    SwitchSession(String),
    ArchiveSession(String),
    RelocateSession {
        session: String,
        env: Option<String>,
        cwd: String,
    },
    ConfigureChat(Value),
    SetDefaultModel {
        provider: String,
        model_id: String,
    },
    TreeNavigate(u64),
    CallContract {
        contract: String,
        method: String,
        args: Value,
    },
    SwitchTheme(String),
    OpenTextOverlay {
        title: String,
        lines: Vec<String>,
    },
    OpenDiffOverlay {
        title: String,
        path: String,
        diff: String,
    },
    OpenBrowser(String),
    CopyText(String),
    Flash(String),
    CloseOverlay,
}

pub(super) type ActionQueue = std::sync::Arc<std::sync::Mutex<VecDeque<PanelAction>>>;

pub(super) fn push_action(queue: &ActionQueue, action: PanelAction) {
    queue
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back(action);
}

pub(super) struct PanelKeys;
impl SelectorKeys for PanelKeys {
    fn matches(&self, data: &str, id: &str) -> bool {
        pi_rs_tui::get_keybindings().matches(data, id)
    }
}

pub(super) struct PanelRenderCtx(pub(super) ThemeRef);
impl pi_rs_p2_tui::RenderContext for PanelRenderCtx {
    fn theme(&self) -> &ThemeRef {
        &self.0
    }
    fn key_text(&self, id: &str) -> String {
        match id {
            "p2.panel.focus" => "F2".into(),
            "tui.select.cancel" => "Esc".into(),
            "app.tools.expand" => "ctrl+o".into(),
            _ => String::new(),
        }
    }
    fn request_render(&self) {}
}

pub(super) fn selector_ctx(theme: &ThemeRef) -> SelectorContext {
    SelectorContext::new(Box::new(PanelRenderCtx(theme.clone())), Box::new(PanelKeys))
}

#[derive(Clone)]
pub(super) struct InteractiveRow {
    pub(super) display: String,
    pub(super) on_enter: Option<PanelAction>,
    pub(super) shortcuts: Vec<(String, PanelAction)>,
}

/// Interactive list `PanelContent` supporting `j`/`k` (and `n`/`p`), `Enter`, and custom shortcut keys.
pub(super) struct InteractiveListPanel {
    header: Vec<String>,
    rows: Vec<InteractiveRow>,
    footer: Vec<String>,
    selected: Rc<RefCell<usize>>,
    actions: ActionQueue,
    theme: ThemeRef,
}

impl InteractiveListPanel {
    pub(super) fn new(
        header: Vec<String>,
        rows: Vec<InteractiveRow>,
        footer: Vec<String>,
        selected: Rc<RefCell<usize>>,
        actions: ActionQueue,
        theme: ThemeRef,
    ) -> Self {
        let clamped = (*selected.borrow()).min(rows.len().saturating_sub(1));
        *selected.borrow_mut() = clamped;
        Self {
            header,
            rows,
            footer,
            selected,
            actions,
            theme,
        }
    }
}

impl Component for InteractiveListPanel {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = self.theme.current();
        let mut out = Vec::new();
        for line in &self.header {
            out.push(truncate_to_width(line, width, true));
        }
        let sel = *self.selected.borrow();
        for (idx, row) in self.rows.iter().enumerate() {
            let prefix = if idx == sel && row.on_enter.is_some() {
                theme.fg("accent", "› ")
            } else {
                "  ".to_string()
            };
            out.push(truncate_to_width(
                &format!("{prefix}{}", row.display),
                width,
                true,
            ));
        }
        for line in &self.footer {
            out.push(truncate_to_width(line, width, true));
        }
        out
    }

    fn handle_input(&mut self, data: &str) {
        let keys = pi_rs_tui::get_keybindings();
        let matches = |k: &str| pi_rs_tui::keys::matches_key_data(data, k);
        if data == "j" || data == "n" || matches("tab") {
            let len = self.rows.len();
            if len > 0 {
                let mut sel = self.selected.borrow_mut();
                *sel = (*sel + 1) % len;
            }
            return;
        }
        if data == "k" || data == "p" || matches("shift+tab") {
            let len = self.rows.len();
            if len > 0 {
                let mut sel = self.selected.borrow_mut();
                *sel = if *sel == 0 { len - 1 } else { *sel - 1 };
            }
            return;
        }
        let sel = *self.selected.borrow();
        let Some(row) = self.rows.get(sel) else {
            return;
        };
        if keys.matches(data, "tui.select.confirm") {
            if let Some(action) = &row.on_enter {
                push_action(&self.actions, action.clone());
            }
            return;
        }
        for (key, action) in &row.shortcuts {
            if matches(key) {
                push_action(&self.actions, action.clone());
                return;
            }
        }
    }
}

impl PanelContent for InteractiveListPanel {}

pub(super) struct FramedOverlay {
    title: String,
    inner: Box<dyn Component>,
    theme: ThemeRef,
    focused: bool,
}

impl FramedOverlay {
    pub(super) fn new(
        title: impl Into<String>,
        inner: Box<dyn Component>,
        theme: ThemeRef,
    ) -> Self {
        Self {
            title: title.into(),
            inner,
            theme,
            focused: true,
        }
    }
}

impl Component for FramedOverlay {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = self.theme.current();
        let rule = theme.fg("border", &"─".repeat(width.max(1)));
        let mut out = vec![
            rule.clone(),
            truncate_to_width(&theme.fg("accent", &self.title), width, true),
        ];
        for line in self.inner.render(width) {
            out.push(truncate_to_width(&line, width, true));
        }
        out.push(truncate_to_width(
            &theme.fg("dim", "↑↓ navigate · Enter confirm · Esc close"),
            width,
            true,
        ));
        out.push(rule);
        out
    }

    fn handle_input(&mut self, data: &str) {
        self.inner.handle_input(data);
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn invalidate(&mut self) {
        self.inner.invalidate();
    }
}

impl Focusable for FramedOverlay {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if let Some(f) = self.inner.as_focusable() {
            f.set_focused(focused);
        }
    }
}

pub(super) fn make_tree_overlay(
    view: &View,
    theme: &ThemeRef,
    actions: &ActionQueue,
) -> Option<FramedOverlay> {
    let tree = view.tree_data.as_ref()?;
    let current = tree["current"].as_u64().map(|c| c as u32);
    let nodes: Vec<TreeSelectorNode> = tree["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| {
            let entry = n["entry"].as_u64()? as u32;
            let parent = n["parent"].as_u64().map(|p| p as u32);
            let conv = n["conversation"].as_u64().unwrap_or(entry as u64) as u32;
            let role = match n["role"].as_str().unwrap_or("user") {
                "assistant" => TreeRole::Assistant,
                _ => TreeRole::User,
            };
            let text = line_text(n["text"].as_str().unwrap_or(""));
            Some(TreeSelectorNode {
                entry,
                parent,
                conversation: conv,
                role,
                text,
            })
        })
        .collect();
    if nodes.is_empty() {
        return None;
    }
    let select_q = actions.clone();
    let rename_q = actions.clone();
    let cancel_q = actions.clone();
    let tree_comp = TreeSelectorComponent::new(
        selector_ctx(theme),
        nodes,
        move |entry| {
            push_action(&select_q, PanelAction::TreeNavigate(entry as u64));
            push_action(&select_q, PanelAction::CloseOverlay);
        },
        move |label| {
            let _ = label;
            push_action(&rename_q, PanelAction::Flash("Tree branch renamed".into()));
        },
        move || {
            push_action(&cancel_q, PanelAction::CloseOverlay);
        },
        current,
    );
    Some(FramedOverlay::new(
        "Conversation Tree",
        Box::new(tree_comp),
        theme.clone(),
    ))
}

pub(super) fn make_fork_overlay(
    view: &View,
    theme: &ThemeRef,
    actions: &ActionQueue,
) -> Option<FramedOverlay> {
    let msgs: Vec<UserMessageItem> = view
        .user_messages()
        .into_iter()
        .map(|(id, text)| UserMessageItem { id, text })
        .collect();
    if msgs.is_empty() {
        return None;
    }
    let session = view.destination().map(|(s, _)| s.clone())?;
    let on_select_q = actions.clone();
    let on_cancel_q = actions.clone();
    let comp = UserMessageSelectorComponent::new(
        selector_ctx(theme),
        msgs,
        move |entry_id| {
            push_action(
                &on_select_q,
                PanelAction::CallContract {
                    contract: "p2.sessions".into(),
                    method: "fork".into(),
                    args: json!({ "session": session, "entryId": entry_id }),
                },
            );
            push_action(&on_select_q, PanelAction::CloseOverlay);
        },
        move || {
            push_action(&on_cancel_q, PanelAction::CloseOverlay);
        },
        None,
    );
    Some(FramedOverlay::new(
        "Fork from message",
        Box::new(comp),
        theme.clone(),
    ))
}

pub(super) fn make_diff_overlay(
    title: String,
    path: String,
    diff: &str,
    theme: &ThemeRef,
    actions: ActionQueue,
) -> pi_rs_p2_tui::TextOverlay {
    let rendered = render_diff(
        diff,
        &theme.current(),
        RenderDiffOptions {
            file_path: Some(path),
        },
    );
    let lines: Vec<String> = rendered.lines().map(str::to_owned).collect();
    pi_rs_p2_tui::TextOverlay::new(
        Box::new(PanelRenderCtx(theme.clone())),
        title,
        lines,
        move || {
            push_action(&actions, PanelAction::CloseOverlay);
        },
    )
}

pub(super) fn make_scoped_models_overlay(view: &View, theme: &ThemeRef) -> FramedOverlay {
    let models: Vec<SelectorModel> = view
        .models_list
        .iter()
        .map(|m| SelectorModel {
            provider: m["provider"].as_str().unwrap_or("unknown").to_string(),
            model_id: m["id"]
                .as_str()
                .or_else(|| m["modelId"].as_str())
                .unwrap_or("unknown")
                .to_string(),
            name: m["name"].as_str().unwrap_or_default().to_string(),
        })
        .collect();
    let enabled: Vec<(String, String)> = view
        .enabled_models
        .iter()
        .filter_map(|m| {
            Some((
                m["provider"].as_str()?.to_string(),
                m["modelId"].as_str()?.to_string(),
            ))
        })
        .collect();
    let comp = ScopedModelsSelectorComponent::new(selector_ctx(theme), models, enabled);
    FramedOverlay::new("Scoped Models", Box::new(comp), theme.clone())
}

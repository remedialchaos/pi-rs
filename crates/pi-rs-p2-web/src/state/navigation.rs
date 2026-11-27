//! Browser URL routing, navigation history, session panel memory, and storage keys
//! (upstream `navigation.ts`, `session-panels.ts`, `sidebar.ts`, `notice-state.ts`).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// `sessionStorage` key for the tab's navigation history trail.
pub const NAV_STORAGE_KEY: &str = "p2-navigation";
/// `sessionStorage` key for per-route scroll positions.
pub const SCROLL_STORAGE_KEY: &str = "p2-navigation-scroll";
/// `localStorage` key for the last open `(session, conversation)` on this device.
pub const LAST_CONVERSATION_STORAGE_KEY: &str = "p2-last-conversation";
/// `localStorage` key mapping session IDs to last known main conversation IDs.
pub const MAINS_STORAGE_KEY: &str = "p2-mains";
/// `localStorage` key remembering open panels per session (LRU up to `SESSION_PANELS_LIMIT`).
pub const SESSION_PANELS_STORAGE_KEY: &str = "p2-session-panels";
/// `localStorage` key for whether the sidebar is collapsed.
pub const SIDEBAR_COLLAPSED_KEY: &str = "pi-sidebar-collapsed";
/// `localStorage` key prefix for dismissed owner notices.
pub const DISMISSED_NOTICE_PREFIX: &str = "pi-owner-notice-dismissed:";
/// Maximum number of session panel entries remembered on one device.
pub const SESSION_PANELS_LIMIT: usize = 200;
/// Maximum number of scroll entries kept in `sessionStorage`.
pub const SCROLL_HISTORY_LIMIT: usize = 200;
/// Media query where the sidebar docks instead of acting as a mobile drawer.
pub const SIDEBAR_DOCKS_MEDIA: &str = "(min-width: 900px)";

/// A modal layer stacked above the route.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Layer {
    Drawer,
    Sheet {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locator: Option<String>,
    },
    Viewer {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locator: Option<String>,
    },
}

/// Active tab in the workspace side/main view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceTab {
    Files,
    Changes,
    Search,
    Comments,
    Terminal,
}

impl WorkspaceTab {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Changes => "changes",
            Self::Search => "search",
            Self::Comments => "comments",
            Self::Terminal => "terminal",
        }
    }
}

/// Selected file inside the workspace view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceFileRoute {
    pub path: String,
    pub view: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// Workspace search filter.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSearch {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<String>,
}

/// Workspace route payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRoute {
    pub tab: WorkspaceTab,
    pub directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<WorkspaceFileRoute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<WorkspaceSearch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
    #[serde(default)]
    pub start_terminal: bool,
}

/// Active transcript activity detail shown in the side panel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityTarget {
    pub row: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
}

/// Canonical browser route state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    #[serde(default)]
    pub new_session: bool,
    pub conversation: u64,
    #[serde(default)]
    pub layers: Vec<Layer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRoute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<ActivityTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
    #[serde(default)]
    pub start_terminal: bool,
}

impl Default for Route {
    fn default() -> Self {
        Self::home()
    }
}

impl Route {
    /// Home route (`/`).
    pub const fn home() -> Self {
        Self {
            session: None,
            page: None,
            new_session: false,
            conversation: 1,
            layers: Vec::new(),
            workspace: None,
            activity: None,
            panel: None,
            terminal: None,
            start_terminal: false,
        }
    }

    /// New-session route (`/new`).
    pub const fn new_session_screen() -> Self {
        Self {
            session: None,
            page: None,
            new_session: true,
            conversation: 1,
            layers: Vec::new(),
            workspace: None,
            activity: None,
            panel: None,
            terminal: None,
            start_terminal: false,
        }
    }

    /// List rollup route (`/list/:page`).
    pub fn list_page(page: impl Into<String>) -> Self {
        Self {
            page: Some(page.into()),
            ..Self::home()
        }
    }

    /// Session conversation route (`/s/:id` or `/s/:id/c/:n`).
    pub fn session_conversation(session: impl Into<String>, conversation: u64) -> Self {
        Self {
            session: Some(session.into()),
            conversation: conversation.max(1),
            ..Self::home()
        }
    }
}

fn percent_encode_segment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            other => {
                use std::fmt::Write;
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(val) = u8::from_str_radix(hex, 16)
        {
            out.push(val);
            i += 3;
            continue;
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Canonical URL pathname for `route` (`/`, `/new`, `/list/:page`, `/s/:id`, `/s/:id/c/:n`).
pub fn route_path(route: &Route) -> String {
    if let Some(page) = &route.page {
        return format!("/list/{}", percent_encode_segment(page));
    }
    if route.new_session {
        return "/new".to_owned();
    }
    let Some(session) = &route.session else {
        return "/".to_owned();
    };
    if route.conversation <= 1 {
        format!("/s/{}", percent_encode_segment(session))
    } else {
        format!(
            "/s/{}/c/{}",
            percent_encode_segment(session),
            route.conversation
        )
    }
}

/// Shareable href for `route`, preserving a top `Layer::Viewer` in `?viewer=&at=`.
pub fn route_href(route: &Route) -> String {
    let base = route_path(route);
    if let Some(Layer::Viewer { name, locator }) = route.layers.last() {
        let mut query = format!("viewer={}", percent_encode_segment(name));
        if let Some(at) = locator
            && !at.is_empty()
        {
            query.push_str("&at=");
            query.push_str(&percent_encode_segment(at));
        }
        format!("{base}?{query}")
    } else {
        base
    }
}

/// Parses `pathname` and `search` (`?viewer=&at=`) into a `Route`.
pub fn path_route(path: &str, search: &str) -> Route {
    let trimmed = path.trim_matches('/');
    let mut route = if trimmed.is_empty() {
        Route::home()
    } else if trimmed == "new" {
        Route::new_session_screen()
    } else {
        let segments: Vec<&str> = trimmed.split('/').collect();
        match segments.as_slice() {
            ["list", page] if !page.is_empty() => Route::list_page(percent_decode(page)),
            ["s", id] if !id.is_empty() => Route::session_conversation(percent_decode(id), 1),
            ["s", id, "c", conv] if !id.is_empty() => {
                let n = conv.parse::<u64>().ok().filter(|&v| v >= 1).unwrap_or(1);
                Route::session_conversation(percent_decode(id), n)
            }
            _ => Route::home(),
        }
    };

    let query = search.strip_prefix('?').unwrap_or(search);
    if !query.is_empty() {
        let mut viewer: Option<String> = None;
        let mut at: Option<String> = None;
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                let key = percent_decode(k);
                let val = percent_decode(v);
                if key == "viewer" && !val.is_empty() {
                    viewer = Some(val);
                } else if key == "at" && !val.is_empty() {
                    at = Some(val);
                }
            }
        }
        if let Some(name) = viewer {
            route.layers.push(Layer::Viewer { name, locator: at });
        }
    }

    route
}

/// Panels remembered per session in `p2-session-panels`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPanels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceRoute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<ActivityTarget>,
    #[serde(default = "default_conv")]
    pub conversation: u64,
}

fn default_conv() -> u64 {
    1
}

/// Remembered panels store (`[(session_id, SessionPanels)]`, LRU ordered).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPanelsStore(pub Vec<(String, SessionPanels)>);

impl SessionPanelsStore {
    pub fn from_json(raw: Option<&str>) -> Self {
        let Some(raw) = raw else {
            return Self::default();
        };
        serde_json::from_str::<Vec<(String, SessionPanels)>>(raw)
            .map(Self)
            .unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.0).unwrap_or_else(|_| "[]".to_owned())
    }

    /// Updates panels for `route.session`, removing empty entries and capping at `SESSION_PANELS_LIMIT`.
    pub fn remember(&mut self, route: &Route) {
        let Some(session) = route.session.as_deref() else {
            return;
        };
        let has_open = route.workspace.is_some()
            || route.terminal.is_some()
            || route.panel.is_some()
            || route.activity.is_some();
        self.0.retain(|(id, _)| id != session);
        if has_open {
            self.0.push((
                session.to_owned(),
                SessionPanels {
                    workspace: route.workspace.clone(),
                    terminal: route.terminal.clone(),
                    panel: route.panel.clone(),
                    activity: route.activity.clone(),
                    conversation: route.conversation,
                },
            ));
            if self.0.len() > SESSION_PANELS_LIMIT {
                let excess = self.0.len() - SESSION_PANELS_LIMIT;
                self.0.drain(..excess);
            }
        }
    }

    /// Looks up remembered panels for `session` and `conversation`, respecting viewport docking flags.
    pub fn panels_for(
        &self,
        session: &str,
        conversation: u64,
        workspace_docks: bool,
        side_panel_docks: bool,
    ) -> SessionPanels {
        let Some((_, kept)) = self.0.iter().find(|(id, _)| id == session) else {
            return SessionPanels::default();
        };
        SessionPanels {
            workspace: if workspace_docks {
                kept.workspace.clone()
            } else {
                None
            },
            terminal: kept.terminal.clone(),
            panel: if side_panel_docks {
                kept.panel.clone()
            } else {
                None
            },
            activity: if side_panel_docks && kept.conversation == conversation {
                kept.activity.clone()
            } else {
                None
            },
            conversation,
        }
    }
}

/// Last conversation record stored in `p2-last-conversation`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastConversation {
    pub session: String,
    pub conversation: u64,
    pub at: u64,
}

pub fn encode_last_conversation(session: &str, conversation: u64, at_ms: u64) -> String {
    serde_json::json!({
        "session": session,
        "conversation": conversation,
        "at": at_ms
    })
    .to_string()
}

pub fn decode_last_conversation(raw: Option<&str>) -> Option<LastConversation> {
    serde_json::from_str::<LastConversation>(raw?).ok()
}

/// Per-tab navigation history stack + scroll memory (`p2-navigation`, `p2-navigation-scroll`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavigationState {
    pub doc: usize,
    pub trail: Vec<Route>,
    #[serde(skip)]
    pub scroll: HashMap<usize, i64>,
}

impl Default for NavigationState {
    fn default() -> Self {
        Self {
            doc: 0,
            trail: vec![Route::home()],
            scroll: HashMap::new(),
        }
    }
}

impl NavigationState {
    pub fn from_initial(route: Route, saved_nav: Option<&str>, saved_scroll: Option<&str>) -> Self {
        let mut state = saved_nav
            .and_then(|raw| serde_json::from_str::<Self>(raw).ok())
            .filter(|s| !s.trail.is_empty() && s.doc < s.trail.len())
            .unwrap_or_else(|| Self {
                doc: 0,
                trail: vec![route.clone()],
                scroll: HashMap::new(),
            });
        state.trail[state.doc] = route;
        if let Some(raw) = saved_scroll
            && let Ok(pairs) = serde_json::from_str::<Vec<(usize, i64)>>(raw)
        {
            for (idx, pos) in pairs {
                state.scroll.insert(idx, pos);
            }
        }
        state
    }

    pub fn current(&self) -> &Route {
        self.trail.get(self.doc).unwrap_or_else(|| &self.trail[0])
    }

    pub fn push(&mut self, next: Route) {
        self.trail.truncate(self.doc + 1);
        self.trail.push(next);
        self.doc = self.trail.len() - 1;
    }

    pub fn replace(&mut self, next: Route) {
        if self.doc < self.trail.len() {
            self.trail[self.doc] = next;
        } else {
            self.trail.push(next);
            self.doc = self.trail.len() - 1;
        }
    }

    /// Applies a browser `popstate` index or falls back to parsing the browser location.
    pub fn on_popstate(&mut self, history_index: Option<usize>, fallback_route: Route) -> Route {
        if let Some(idx) = history_index
            && idx < self.trail.len()
        {
            self.doc = idx;
            return self.trail[idx].clone();
        }
        self.replace(fallback_route.clone());
        fallback_route
    }

    pub fn record_scroll(&mut self, y: i64) {
        self.scroll.insert(self.doc, y);
    }

    pub fn scroll_for_current(&self) -> Option<i64> {
        self.scroll.get(&self.doc).copied()
    }

    pub fn encode_nav(&self) -> String {
        serde_json::json!({ "doc": self.doc, "trail": self.trail }).to_string()
    }

    pub fn encode_scroll(&self) -> String {
        let mut pairs: Vec<(usize, i64)> = self.scroll.iter().map(|(&k, &v)| (k, v)).collect();
        pairs.sort_by_key(|(k, _)| *k);
        if pairs.len() > SCROLL_HISTORY_LIMIT {
            pairs.drain(..pairs.len() - SCROLL_HISTORY_LIMIT);
        }
        serde_json::to_string(&pairs).unwrap_or_else(|_| "[]".to_owned())
    }
}

/// Full localStorage key for a dismissed owner notice (`pi-owner-notice-dismissed:<id>`).
pub fn dismissed_notice_key(id: &str) -> String {
    format!("{DISMISSED_NOTICE_PREFIX}{id}")
}

/// Whether a dev supervisor/watcher notice message represents a success result.
pub fn is_dev_success_result(message: &str) -> bool {
    message == "Reloaded"
        || message.starts_with("Reloaded ")
        || message == "Restarted server"
        || message == "No dirty plugins"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_paths_and_parsing_cover_all_five_shapes_and_viewer_query() {
        let home = path_route("/", "");
        assert_eq!(home, Route::home());
        assert_eq!(route_path(&home), "/");

        let new_s = path_route("/new", "");
        assert!(new_s.new_session);
        assert_eq!(route_path(&new_s), "/new");

        let list = path_route("/list/active%20work", "");
        assert_eq!(list.page.as_deref(), Some("active work"));
        assert_eq!(route_path(&list), "/list/active%20work");

        let sess = path_route("/s/abc-123", "");
        assert_eq!(sess.session.as_deref(), Some("abc-123"));
        assert_eq!(sess.conversation, 1);
        assert_eq!(route_path(&sess), "/s/abc-123");

        let branch = path_route("/s/abc-123/c/4", "?viewer=file&at=src%2Flib.rs");
        assert_eq!(branch.session.as_deref(), Some("abc-123"));
        assert_eq!(branch.conversation, 4);
        assert_eq!(
            branch.layers,
            vec![Layer::Viewer {
                name: "file".into(),
                locator: Some("src/lib.rs".into()),
            }]
        );
        assert_eq!(route_path(&branch), "/s/abc-123/c/4");
        assert_eq!(
            route_href(&branch),
            "/s/abc-123/c/4?viewer=file&at=src%2Flib.rs"
        );
    }

    #[test]
    fn navigation_history_push_replace_and_popstate() {
        let mut nav = NavigationState::default();
        nav.push(Route::session_conversation("s1", 1));
        nav.push(Route::session_conversation("s1", 2));
        assert_eq!(nav.doc, 2);
        assert_eq!(nav.current().conversation, 2);

        let popped = nav.on_popstate(Some(1), Route::home());
        assert_eq!(popped.session.as_deref(), Some("s1"));
        assert_eq!(popped.conversation, 1);
        assert_eq!(nav.doc, 1);

        nav.replace(Route::list_page("pinned"));
        assert_eq!(nav.current().page.as_deref(), Some("pinned"));

        nav.push(Route::new_session_screen());
        assert_eq!(nav.trail.len(), 3);
        assert!(nav.current().new_session);
    }

    #[test]
    fn session_panels_store_remembers_and_respects_docking() {
        let mut store = SessionPanelsStore::default();
        let mut r = Route::session_conversation("s1", 2);
        r.panel = Some("models".into());
        r.terminal = Some("t1".into());
        r.activity = Some(ActivityTarget {
            row: "r1".into(),
            step: None,
        });
        store.remember(&r);

        let desktop = store.panels_for("s1", 2, true, true);
        assert_eq!(desktop.panel.as_deref(), Some("models"));
        assert_eq!(desktop.terminal.as_deref(), Some("t1"));
        assert!(desktop.activity.is_some());

        let mobile = store.panels_for("s1", 2, false, false);
        assert_eq!(mobile.panel, None);
        assert_eq!(mobile.terminal.as_deref(), Some("t1"));
        assert_eq!(mobile.activity, None);
    }

    #[test]
    fn storage_key_constants_and_notice_helpers_match_upstream() {
        assert_eq!(NAV_STORAGE_KEY, "p2-navigation");
        assert_eq!(SCROLL_STORAGE_KEY, "p2-navigation-scroll");
        assert_eq!(LAST_CONVERSATION_STORAGE_KEY, "p2-last-conversation");
        assert_eq!(MAINS_STORAGE_KEY, "p2-mains");
        assert_eq!(SESSION_PANELS_STORAGE_KEY, "p2-session-panels");
        assert_eq!(SIDEBAR_COLLAPSED_KEY, "pi-sidebar-collapsed");
        assert_eq!(DISMISSED_NOTICE_PREFIX, "pi-owner-notice-dismissed:");
        assert_eq!(
            dismissed_notice_key("log:1"),
            "pi-owner-notice-dismissed:log:1"
        );
        assert!(is_dev_success_result("Reloaded 2 plugins"));
        assert!(is_dev_success_result("Restarted server"));
        assert!(!is_dev_success_result("Compile failed"));
    }
}

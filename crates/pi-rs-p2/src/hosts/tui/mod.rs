mod actions;
mod admission;
mod builtin_panels;
pub mod clipboard;
mod facets;
mod grants;
pub mod plugins;
mod preferences;
#[cfg(test)]
mod pty;
mod radius;
mod terminal_colors;
mod view;
mod when;

use crate::runtime::local_owner::*;
use facets::{
    ActionQueue, PanelAction, PanelRenderCtx, make_diff_overlay, make_fork_overlay,
    make_tree_overlay, push_action,
};
use pi_rs_chord::node::http::connect_websocket;
use pi_rs_p2_tui::{
    CORE_COMMANDS, CommandInfo, CommandPalette, FileCompletionProvider, FileEntry, FileSource,
    KeybindingsOverlay, RenderContext, TextOverlay, ThemeRef,
    components::{BuiltInHeader, HeaderSource, spread},
    host::TuiClient,
    hotkeys_report_text, open_settings,
    tools::quiet_tools_setting_field,
};
use pi_rs_tui::{
    AutocompleteItem, CombinedAutocompleteProvider, Component, ComponentRef, Editor,
    EditorAppearance, EditorAutocomplete, EditorComponent, EditorHistory, EditorOptions, Focusable,
    OverlayAnchor, OverlayHandle, OverlayOptions, ProcessTerminal, SizeValue, SlashCommand, Tui,
    TuiInputListenerResult, fuzzy_filter,
};
use reqwest::{Client, Url, header};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    env,
    error::Error,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex as AsyncMutex;

const DOUBLE_CTRL_C_MS: Duration = Duration::from_millis(500);
const DOUBLE_ESCAPE_MS: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliOptions {
    theme: Option<String>,
    no_themes: bool,
    address: Option<String>,
    join_link: Option<Url>,
    owner_link: Option<String>,
    explicit_owner: bool,
    radius_server: Option<Option<String>>,
}

fn parse_cli_args(args_iter: impl Iterator<Item = String>) -> Result<CliOptions, Box<dyn Error>> {
    let mut theme = None;
    let mut no_themes = false;
    let mut address = None;
    let mut join_link = None;
    let mut explicit_owner = false;
    let mut owner_link = env::var("P2_TUI_OWNER_LINK").ok().filter(|s| !s.is_empty());
    let mut radius_server: Option<Option<String>> = None;
    let mut args = args_iter.peekable();
    while let Some(arg) = args.next() {
        if arg == "--theme" {
            theme = Some(
                args.next()
                    .filter(|s| !s.is_empty())
                    .ok_or("--theme needs a value")?,
            );
            continue;
        }
        if arg == "--no-themes" {
            no_themes = true;
            continue;
        }
        if arg == "--owner-link" {
            explicit_owner = true;
            owner_link = Some(
                args.next()
                    .filter(|s| !s.is_empty())
                    .ok_or("--owner-link needs a value")?,
            );
            continue;
        }
        if arg == "--join" {
            join_link = Some(
                Url::parse(&args.next().ok_or("--join needs a share link")?)
                    .map_err(|_| "Invalid share link")?,
            );
            continue;
        }
        if arg == "--radius" {
            let next_id = args
                .peek()
                .filter(|next| radius::is_radius_server_id(next))
                .cloned();
            if next_id.is_some() {
                args.next();
            }
            radius_server = Some(next_id);
            continue;
        }
        if arg.starts_with('-') {
            return Err(format!("Unknown TUI option: {arg}").into());
        }
        address = Some(arg);
    }
    if join_link.is_some() && explicit_owner {
        return Err("Use either --join or --owner-link".into());
    }
    if radius_server.is_some() && (address.is_some() || join_link.is_some() || explicit_owner) {
        return Err("--radius signs in through Radius: no URL, owner link or share link".into());
    }
    if join_link.is_some() || radius_server.is_some() {
        owner_link = None;
    }
    Ok(CliOptions {
        theme,
        no_themes,
        address,
        join_link,
        owner_link,
        explicit_owner,
        radius_server,
    })
}

pub async fn run() -> Result<(), Box<dyn Error>> {
    let default_address = format!(
        "http://127.0.0.1:{}",
        env::var("PORT").unwrap_or_else(|_| "8788".into())
    );
    let opts = parse_cli_args(
        env::args_os()
            .skip(2)
            .map(|arg| arg.to_string_lossy().into_owned()),
    )?;
    let home = env::home_dir().ok_or("Cannot determine home directory")?;
    let config_dir = env::var_os("P2_TUI_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".pi/p2-tui"));
    let config_path = config_dir.join("config.json");

    let radius_bridge = if let Some(server_id) = opts.radius_server {
        Some(radius::connect_radius(&config_dir, server_id.as_deref()).await?)
    } else {
        None
    };

    let (url, origin, poll_interval, local, client) = if let Some(ref bridge) = radius_bridge {
        let c = Client::builder().timeout(Duration::from_secs(30)).build()?;
        (
            bridge.local_origin.clone(),
            bridge.logical_origin.clone(),
            radius::RADIUS_POLL_MS,
            false,
            c,
        )
    } else {
        let address = opts
            .address
            .or_else(|| {
                opts.join_link
                    .as_ref()
                    .map(|link| link.origin().ascii_serialization())
            })
            .unwrap_or(default_address);
        let url = Url::parse(&address)?;
        let local = url.scheme() == "http"
            && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if (!local && url.scheme() != "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("Expected a secure origin or HTTP loopback".into());
        }
        let origin = url.origin().ascii_serialization();
        let c = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()?;
        (url, origin, Duration::from_secs(1), local, c)
    };

    let mut config = admission::load(&config_path)?;
    let mut guest = false;
    let mut local_admission = false;
    let mut preferred = None;
    let mut cookie = String::new();

    if radius_bridge.is_none() {
        let explicit = match (opts.join_link, opts.owner_link) {
            (Some(link), _) => Some((link, true)),
            (None, Some(link)) => Some((
                Url::parse(link.trim()).map_err(|_| "Invalid owner link")?,
                false,
            )),
            _ => None,
        };
        let resolved_cookie = if let Some((link, is_guest)) = explicit {
            guest = is_guest;
            let (admitted, session) =
                admission::redeem(&client, &url, &link, &mut config, &config_path, guest).await?;
            preferred = session;
            Some(admitted)
        } else if let Some(saved) = config["connections"][&origin]["cookie"].as_str() {
            let check = client
                .get(url.join("/owner/check")?)
                .header(header::ORIGIN, &origin)
                .header(header::COOKIE, saved)
                .send()
                .await?;
            if check.status().is_success() {
                if check.json::<Value>().await?["owner"] != true {
                    return Err("Invalid owner-access response".into());
                }
                Some(saved.to_owned())
            } else if matches!(check.status().as_u16(), 401 | 403) {
                None
            } else {
                return Err("Cannot check owner access".into());
            }
        } else {
            None
        };

        let resolved_cookie = if resolved_cookie.is_none()
            && !local
            && let Some(saved) = config["connections"][&origin]["guest"]["cookie"].as_str()
        {
            let check = client
                .head(url.join("/__p2_tui")?)
                .header(header::ORIGIN, &origin)
                .header(header::COOKIE, saved)
                .send()
                .await?;
            if check.status().is_success() {
                guest = true;
                preferred = config["connections"][&origin]["guest"]["session"]
                    .as_str()
                    .map(str::to_owned);
                Some(saved.to_owned())
            } else if matches!(check.status().as_u16(), 401 | 403) {
                admission::save(
                    &config_path,
                    &origin,
                    "guest",
                    None,
                    &config["connections"][&origin]["guest"],
                )?;
                if let Some(connection) = config["connections"][&origin].as_object_mut() {
                    connection.remove("guest");
                }
                None
            } else {
                return Err("Cannot check guest access".into());
            }
        } else {
            resolved_cookie
        };

        cookie = match resolved_cookie {
            Some(c) => c,
            None => {
                let local_cookie = if local {
                    match local_sign_in(&client, &url, &home).await {
                        Ok(c) => Some(c),
                        Err(error) => {
                            eprintln!("Local sign-in unavailable: {error}");
                            None
                        }
                    }
                } else {
                    None
                };
                if let Some(c) = local_cookie {
                    local_admission = true;
                    c
                } else {
                    eprint!("Owner or share link: ");
                    let mut input = String::new();
                    if std::io::stdin().read_line(&mut input)? == 0 {
                        return Err("Sign-in requires an owner or share link".into());
                    }
                    let link = Url::parse(input.trim()).map_err(|_| "Invalid sign-in link")?;
                    guest = link.path().starts_with("/join/");
                    let (c, session) =
                        admission::redeem(&client, &url, &link, &mut config, &config_path, guest)
                            .await?;
                    preferred = session;
                    c
                }
            }
        };
    }

    let mut socket = url.join("/chord?client=tui")?;
    socket
        .set_scheme(if url.scheme() == "http" { "ws" } else { "wss" })
        .map_err(|_| "Invalid socket origin")?;

    loop {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::ORIGIN, origin.parse()?);
        if !cookie.is_empty() {
            let mut credential = header::HeaderValue::from_str(&cookie)?;
            credential.set_sensitive(true);
            headers.insert(header::COOKIE, credential);
        }
        let socket_open = tokio::time::timeout(
            Duration::from_secs(10),
            connect_websocket(socket.as_str(), headers, 4 * 1024 * 1024, 4 * 1024 * 1024),
        );
        let result = match socket_open.await? {
            Ok(link) => {
                let prefs = preferences::Preferences::load(
                    &home,
                    &config_dir,
                    opts.theme.clone(),
                    opts.no_themes,
                );
                let local_fallback = (local && !guest && radius_bridge.is_none()).then(|| {
                    env::var_os("P2_DATA_DIR")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| home.join(".pi/p2"))
                        .join("plugins")
                });
                let remote = plugins::RemoteSource {
                    client: client.clone(),
                    base_url: url.clone(),
                    origin: origin.clone(),
                    cookie: cookie.clone(),
                    poll_interval,
                };
                let plugins =
                    plugins::Plugins::load_remote_or_local(remote, local_fallback).await?;
                mount(
                    view::View::connect(link, preferred.clone())?,
                    prefs,
                    plugins,
                    origin.clone(),
                    if guest { "guest" } else { "owner" },
                )
                .await
            }
            Err(error) => Err(error.into()),
        };
        let Err(error) = result else {
            return Ok(());
        };
        if radius_bridge.is_some() {
            return Err(error);
        }

        let check = if guest {
            client.head(url.join("/__p2_tui")?)
        } else {
            client.get(url.join("/owner/check")?)
        };
        if !check
            .header(header::ORIGIN, &origin)
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .is_ok_and(|r| matches!(r.status().as_u16(), 401 | 403))
        {
            return Err(error);
        }
        if local_admission {
            local_admission = false;
            if let Ok(fresh) = local_sign_in(&client, &url, &home).await {
                cookie = fresh;
                continue;
            }
        }
        let field = if guest { "guest" } else { "cookie" };
        let old = &config["connections"][&origin][field];
        if guest || old.as_str() == Some(&cookie) {
            admission::save(&config_path, &origin, field, None, old)?;
            if let Some(connection) = config["connections"][&origin].as_object_mut() {
                connection.remove(field);
            }
        }
        eprintln!(
            "{}",
            if guest {
                "This share has ended."
            } else {
                "Signed out on this device."
            }
        );
        loop {
            eprint!("Owner or share link (Enter to quit): ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            if input.trim().is_empty() {
                return Ok(());
            }
            let Ok(link) = Url::parse(input.trim()) else {
                eprintln!("Invalid sign-in link");
                continue;
            };
            match admission::redeem(
                &client,
                &url,
                &link,
                &mut config,
                &config_path,
                link.path().starts_with("/join/"),
            )
            .await
            {
                Ok((new_cookie, session)) => {
                    guest = link.path().starts_with("/join/");
                    cookie = new_cookie;
                    preferred = session;
                    break;
                }
                Err(problem) => eprintln!("{problem}"),
            }
        }
    }
}

fn response_cookie(response: &reqwest::Response) -> Result<String, Box<dyn Error>> {
    if !response.status().is_success() {
        return Err("Owner sign-in failed".into());
    }
    let pairs = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| {
            let value = value.to_str()?;
            Ok(value.split_once(';').map_or(value, |(pair, _)| pair))
        })
        .collect::<Result<Vec<_>, header::ToStrError>>()?;
    let cookie = pairs.join("; ");
    if cookie.is_empty() {
        return Err("Missing owner cookie".into());
    }
    Ok(cookie)
}

struct HeaderRenderCtx(ThemeRef);

impl RenderContext for HeaderRenderCtx {
    fn theme(&self) -> &ThemeRef {
        &self.0
    }
    fn key_text(&self, id: &str) -> String {
        match id {
            "app.tools.expand" => "ctrl+o".into(),
            "app.interrupt" => "escape".into(),
            "app.exit" => "ctrl+c".into(),
            "p2.panel.focus" => "F2".into(),
            "p2.sessions" => "ctrl+r".into(),
            _ => String::new(),
        }
    }
    fn request_render(&self) {}
}

struct TranscriptPane {
    view: Rc<RefCell<view::View>>,
    theme: ThemeRef,
}

impl Component for TranscriptPane {
    fn render(&self, width: usize) -> Vec<String> {
        self.view
            .borrow()
            .render_transcript_lines(&self.theme, width)
    }
}

struct EditorDock {
    editor: Rc<RefCell<Editor>>,
    theme: ThemeRef,
    footer_left: Rc<RefCell<String>>,
    footer_right: Rc<RefCell<String>>,
}

impl Component for EditorDock {
    fn render(&self, width: usize) -> Vec<String> {
        let width = width.max(1);
        let theme = self.theme.current();
        let mut lines = Vec::new();
        for line in self.editor.borrow().render(width) {
            lines.push(pi_rs_tui::truncate_to_width(&line, width, true));
        }
        let left = theme.fg("dim", &self.footer_left.borrow());
        let right = theme.fg("dim", &self.footer_right.borrow());
        lines.push(spread(&left, &right, width, 2));
        lines
    }

    fn handle_input(&mut self, data: &str) {
        self.editor.borrow_mut().handle_input(data);
    }

    fn invalidate(&mut self) {
        self.editor.borrow_mut().invalidate();
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for EditorDock {
    fn is_focused(&self) -> bool {
        self.editor.borrow().is_focused()
    }

    fn set_focused(&mut self, focused: bool) {
        self.editor.borrow_mut().set_focused(focused);
    }
}

struct BoxedComponent(Box<dyn Component>);

impl Component for BoxedComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.0.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        self.0.handle_input(data);
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        self.0.as_focusable()
    }

    fn invalidate(&mut self) {
        self.0.invalidate();
    }
}

struct LocalFileSource {
    cwd: Arc<std::sync::Mutex<String>>,
}

impl FileSource for LocalFileSource {
    fn complete(&self, query: &str, limit: usize) -> (Vec<AutocompleteItem>, bool) {
        let root = PathBuf::from(self.cwd.lock().unwrap_or_else(|e| e.into_inner()).as_str());
        let mut paths = Vec::new();
        let mut stack = vec![(root.clone(), String::new())];
        let mut truncated = false;
        while let Some((dir, rel)) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if paths.len() >= 500 {
                    truncated = true;
                    break;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || name == "target" || name == "node_modules" {
                    continue;
                }
                let child_rel = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                };
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push((entry.path(), child_rel));
                } else {
                    paths.push(child_rel);
                }
            }
            if truncated {
                break;
            }
        }
        let matched = if query.is_empty() {
            paths.into_iter().take(limit).collect::<Vec<_>>()
        } else {
            let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
            fuzzy_filter(&refs, query, |s| (*s).to_string())
                .into_iter()
                .take(limit)
                .map(str::to_owned)
                .collect()
        };
        let items = matched
            .into_iter()
            .map(|p| {
                let value = format!("@{p}");
                AutocompleteItem {
                    label: value.clone(),
                    value,
                    description: None,
                }
            })
            .collect();
        (items, truncated)
    }

    fn list(&self, path: &str) -> Vec<FileEntry> {
        let base = PathBuf::from(self.cwd.lock().unwrap_or_else(|e| e.into_inner()).as_str());
        let target = if path.is_empty() {
            base
        } else {
            base.join(path)
        };
        let Ok(entries) = std::fs::read_dir(target) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|entry| FileEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                dir: entry.file_type().is_ok_and(|t| t.is_dir()),
            })
            .collect()
    }
}

#[derive(Clone, Copy)]
enum LauncherKeyEvent {
    CtrlC,
    Palette,
    ToggleExpand,
    ToggleThinking,
    ExternalEditor,
    OpenResume,
    Interrupt,
}

fn build_slash_commands() -> Vec<SlashCommand> {
    builtin_panels::BuiltinPanels::all_slash_metadata()
        .into_iter()
        .map(|(name, desc, hint)| SlashCommand {
            name: name.to_string(),
            description: Some(desc.to_string()),
            argument_hint: hint.map(str::to_string),
            get_argument_completions: None,
        })
        .collect()
}

fn build_command_palette_entries(user_keys: &pi_rs_p2_tui::KeybindingsConfig) -> Vec<CommandInfo> {
    let definitions = pi_rs_p2_tui::builtin_keybindings(Default::default());
    let mut list: Vec<CommandInfo> = CORE_COMMANDS
        .iter()
        .map(|cmd| {
            let keys = user_keys
                .get(cmd.id)
                .or_else(|| definitions.get(cmd.id).map(|d| &d.default_keys))
                .map(|k| k.as_slice().to_vec())
                .unwrap_or_default();
            CommandInfo {
                id: cmd.id.to_string(),
                label: cmd.title.to_string(),
                description: String::new(),
                category: cmd.category.to_string(),
                keys,
                slash: cmd.slash.map(str::to_string),
                argument_hint: None,
                when: None,
                palette: cmd.palette,
            }
        })
        .collect();
    for (name, desc, hint) in builtin_panels::BuiltinPanels::all_slash_metadata() {
        if list.iter().any(|c| c.slash.as_deref() == Some(name)) {
            continue;
        }
        list.push(CommandInfo {
            id: format!("slash.{name}"),
            label: desc.to_string(),
            description: desc.to_string(),
            category: "Plugin".to_string(),
            keys: Vec::new(),
            slash: Some(name.to_string()),
            argument_hint: hint.map(str::to_string),
            when: None,
            palette: true,
        });
    }
    list
}

fn open_overlay(
    client: &mut TuiClient<pi_rs_tui::TuiAltScreen>,
    active: &mut Option<OverlayHandle>,
    overlay_open: &Cell<bool>,
    component: ComponentRef,
    width_pct: usize,
) {
    if let Some(prev) = active.take() {
        client.screen.base.hide_overlay(prev);
    }
    let handle = client.screen.base.show_overlay(
        component,
        OverlayOptions {
            width: Some(SizeValue::Percent(width_pct as f64)),
            anchor: OverlayAnchor::TopCenter,
            offset_y: 1,
            ..Default::default()
        },
    );
    *active = Some(handle);
    overlay_open.set(true);
    client.screen.base.request_render(true);
}

fn close_overlay(
    client: &mut TuiClient<pi_rs_tui::TuiAltScreen>,
    active: &mut Option<OverlayHandle>,
    overlay_open: &Cell<bool>,
    field: &ComponentRef,
) {
    if let Some(handle) = active.take() {
        client.screen.base.hide_overlay(handle);
    }
    overlay_open.set(false);
    if !client.side_panel_focused() {
        client.screen.base.set_focus(Some(field.clone()));
    }
    client.screen.base.request_render(true);
}

fn make_overlay_ctx(theme: &ThemeRef) -> Box<dyn RenderContext> {
    Box::new(PanelRenderCtx(theme.clone()))
}

async fn mount(
    mut view_init: view::View,
    mut prefs: preferences::Preferences,
    mut plugins: Option<plugins::Plugins>,
    origin: String,
    role: &str,
) -> Result<(), Box<dyn Error>> {
    view_init.configure_display(prefs.quiet_tools_enabled(), prefs.output_pad(), None);
    let mode = prefs.theme.color_mode;
    let terminal = prefs.theme.terminal.clone();
    let theme = ThemeRef::new(prefs.theme.clone());
    let hardware_cursor = prefs.show_hardware_cursor();

    let hints_theme = theme.clone();
    let header_component = BuiltInHeader::new(HeaderSource {
        ctx: Box::new(HeaderRenderCtx(theme.clone())),
        origin: Some(origin.clone()),
        role: Some(role.to_owned()),
        connected: true,
        expanded: false,
        hints: Box::new(move || {
            let hint_theme = hints_theme.current();
            vec![
                format!(
                    "{} {}",
                    hint_theme.fg("dim", "escape"),
                    hint_theme.fg("muted", "interrupt")
                ),
                format!(
                    "{} {}",
                    hint_theme.fg("dim", "ctrl+c"),
                    hint_theme.fg("muted", "clear")
                ),
                format!(
                    "{} {}",
                    hint_theme.fg("dim", "/"),
                    hint_theme.fg("muted", "commands")
                ),
                format!(
                    "{} {}",
                    hint_theme.fg("dim", "F1"),
                    hint_theme.fg("muted", "palette")
                ),
                format!(
                    "{} {}",
                    hint_theme.fg("dim", "F2"),
                    hint_theme.fg("muted", "panel")
                ),
                format!(
                    "{} {}",
                    hint_theme.fg("dim", "ctrl+r"),
                    hint_theme.fg("muted", "sessions")
                ),
            ]
        }),
        expanded_hints: Box::new(Vec::new),
    });
    let header = ComponentRef::new(header_component);

    let view = Rc::new(RefCell::new(view_init));
    let root = ComponentRef::new(TranscriptPane {
        view: view.clone(),
        theme: theme.clone(),
    });
    let submit: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let posted = submit.clone();
    let cwd_ref = Arc::new(std::sync::Mutex::new(view.borrow().active_cwd()));
    let slash_provider = Arc::new(CombinedAutocompleteProvider::new(
        build_slash_commands(),
        PathBuf::from("."),
        None,
    ));
    let file_source: Arc<AsyncMutex<Option<Arc<dyn FileSource>>>> =
        Arc::new(AsyncMutex::new(Some(Arc::new(LocalFileSource {
            cwd: cwd_ref.clone(),
        }))));
    let autocomplete = FileCompletionProvider::new(
        slash_provider,
        file_source,
        |_| {},
        || Some("Pi".to_string()),
    );
    let mut editor = Editor::new(
        Box::new(|| 24),
        Rc::new(|| {}),
        theme.editor(),
        EditorOptions {
            padding_x: prefs.editor_padding_x(),
            autocomplete_max_visible: prefs.autocomplete_max_visible(),
        },
    );
    editor.set_autocomplete_provider(Box::new(autocomplete));
    editor.set_on_submit(Some(Box::new(move |text| {
        *posted.borrow_mut() = Some(text.to_owned());
    })));
    let editor = Rc::new(RefCell::new(editor));
    let footer_left = Rc::new(RefCell::new(view.borrow().title().to_owned()));
    let footer_right = Rc::new(RefCell::new("p2 tui".to_owned()));
    let field = ComponentRef::new(EditorDock {
        editor: editor.clone(),
        theme: theme.clone(),
        footer_left: footer_left.clone(),
        footer_right: footer_right.clone(),
    });
    let mut client = TuiClient::mount_panel(
        ProcessTerminal::new(),
        header,
        root,
        field.clone(),
        prefs.mouse,
    );
    client.screen.base.set_show_hardware_cursor(hardware_cursor);
    if let Some(ref path) = prefs.theme_path
        && let Err(error) = client.watch_theme(path, theme.clone(), mode, terminal)
    {
        eprintln!("Live theme reload unavailable: {error}");
    }

    let color_probe = Rc::new(RefCell::new(terminal_colors::TerminalColorProbe::new(
        None, 100,
    )));
    let mut prefs_watcher = preferences::PrefsWatcher::from_prefs(&prefs);
    let done = Rc::new(Cell::new(false));
    let grants = Rc::new(RefCell::new(grants::CopyGrant::default()));
    let mut copy_key = grants::CopyGrant::listener(grants.clone(), &prefs.keys, editor.clone());
    let panel_actions: ActionQueue = Arc::new(std::sync::Mutex::new(VecDeque::new()));
    let mut builtin_panels = builtin_panels::BuiltinPanels::new(panel_actions.clone());
    builtin_panels.set_theme_info(
        prefs.current_theme_name().to_owned(),
        prefs.available_themes(),
    );

    let shortcuts = Rc::new(RefCell::new(
        plugins
            .as_ref()
            .map(|p| p.shortcuts(&prefs.keys))
            .unwrap_or_default(),
    ));
    let shortcut_table = shortcuts.clone();
    let command = Rc::new(RefCell::new(None));
    let shortcut_command = command.clone();
    let panel_open = Rc::new(Cell::new(false));
    let panel_focus = Rc::new(Cell::new(false));
    let overlay_open = Rc::new(Cell::new(false));
    let open = panel_open.clone();
    let focused = panel_focus.clone();
    let overlay_flag = overlay_open.clone();
    let focus_change = Rc::new(Cell::new(None));
    let change = focus_change.clone();
    let chat = Rc::new(Cell::new(false));
    let session = chat.clone();
    let input_editor = editor.clone();
    let key_queue: Rc<RefCell<VecDeque<LauncherKeyEvent>>> = Rc::new(RefCell::new(VecDeque::new()));
    let key_queue_in = key_queue.clone();
    let probe_in = color_probe.clone();
    let pending_probe_writes: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let probe_writes_in = pending_probe_writes.clone();
    let exit_flag = done.clone();

    client.screen.base.add_input_listener(Box::new(move |data| {
        let (probe_consumed, follow_up) = probe_in.borrow_mut().consume(data);
        if let Some(seq) = follow_up {
            probe_writes_in.borrow_mut().push(seq);
        }
        if probe_consumed {
            return TuiInputListenerResult {
                consume: true,
                data: None,
            };
        }
        if overlay_flag.get() {
            return TuiInputListenerResult {
                consume: false,
                data: None,
            };
        }
        let matches = |key| pi_rs_tui::keys::matches_key_data(data, key);
        if matches("ctrl+c") {
            key_queue_in.borrow_mut().push_back(LauncherKeyEvent::CtrlC);
            return TuiInputListenerResult {
                consume: true,
                data: None,
            };
        }
        if matches("ctrl+d") && input_editor.borrow().text().is_empty() {
            exit_flag.set(true);
            return TuiInputListenerResult {
                consume: true,
                data: None,
            };
        }
        if matches("f1") {
            key_queue_in
                .borrow_mut()
                .push_back(LauncherKeyEvent::Palette);
            return TuiInputListenerResult {
                consume: true,
                data: None,
            };
        }
        if matches("ctrl+o") {
            key_queue_in
                .borrow_mut()
                .push_back(LauncherKeyEvent::ToggleExpand);
            return TuiInputListenerResult {
                consume: true,
                data: None,
            };
        }
        if matches("ctrl+t") && !focused.get() {
            key_queue_in
                .borrow_mut()
                .push_back(LauncherKeyEvent::ToggleThinking);
            return TuiInputListenerResult {
                consume: true,
                data: None,
            };
        }
        if matches("ctrl+g") {
            key_queue_in
                .borrow_mut()
                .push_back(LauncherKeyEvent::ExternalEditor);
            return TuiInputListenerResult {
                consume: true,
                data: None,
            };
        }
        if matches("ctrl+r") {
            key_queue_in
                .borrow_mut()
                .push_back(LauncherKeyEvent::OpenResume);
            return TuiInputListenerResult {
                consume: true,
                data: None,
            };
        }
        if matches("escape") {
            if focused.get() {
                change.set(Some(false));
                return TuiInputListenerResult {
                    consume: true,
                    data: None,
                };
            }
            if !input_editor.borrow().is_showing_autocomplete() {
                key_queue_in
                    .borrow_mut()
                    .push_back(LauncherKeyEvent::Interrupt);
                return TuiInputListenerResult {
                    consume: true,
                    data: None,
                };
            }
        }
        if open.get() && matches("f2") {
            change.set(Some(!focused.get()));
            return TuiInputListenerResult {
                consume: true,
                data: None,
            };
        }
        if copy_key(data) {
            return TuiInputListenerResult {
                consume: true,
                data: None,
            };
        }
        let empty = input_editor.borrow().text().is_empty();
        if let Some((_, id, _)) = shortcut_table.borrow().iter().find(|(key, _, when)| {
            matches(key)
                && when.matches(&command_context(
                    session.get(),
                    open.get(),
                    focused.get(),
                    empty,
                ))
        }) {
            *shortcut_command.borrow_mut() = Some((id.clone(), String::new()));
            return TuiInputListenerResult {
                consume: true,
                data: None,
            };
        }
        TuiInputListenerResult {
            consume: false,
            data: None,
        }
    }));

    client.screen.start()?;
    let start_seq = color_probe.borrow_mut().start_sequence();
    client.screen.base.terminal.write(&start_seq);

    let mut active_overlay: Option<OverlayHandle> = None;
    let mut last_ctrl_c: Option<Instant> = None;
    let mut last_escape: Option<Instant> = None;
    let mut last_theme_version = theme.version();
    let mut drafts: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut destination: Option<(String, u64)> = None;

    let result: Result<(), Box<dyn Error>> = loop {
        for seq in pending_probe_writes.borrow_mut().drain(..) {
            client.screen.base.terminal.write(&seq);
        }
        if let Some(updated_terminal) = color_probe.borrow_mut().poll_timeout() {
            prefs.update_terminal(updated_terminal, &theme, &mut client);
        }
        if let Some(write) = grants.borrow_mut().take() {
            client.screen.base.terminal.write(&write);
        }
        if prefs_watcher.poll_changed() {
            prefs = prefs.reload();
            view.borrow_mut().configure_display(
                prefs.quiet_tools_enabled(),
                prefs.output_pad(),
                None,
            );
            client
                .screen
                .base
                .set_show_hardware_cursor(prefs.show_hardware_cursor());
            editor.borrow_mut().set_padding_x(prefs.editor_padding_x());
            editor
                .borrow_mut()
                .set_autocomplete_max_visible(prefs.autocomplete_max_visible());
            client.set_theme(&theme, prefs.theme.clone());
            *shortcuts.borrow_mut() = plugins
                .as_ref()
                .map(|p| p.shortcuts(&prefs.keys))
                .unwrap_or_default();
        }
        if done.get() {
            break Ok(());
        }
        if let Some(focus) = focus_change.take() {
            client.focus_side_panel(focus);
        }

        // Drain launcher key events.
        while let Some(ev) = key_queue.borrow_mut().pop_front() {
            match ev {
                LauncherKeyEvent::CtrlC => {
                    let now = Instant::now();
                    if last_ctrl_c.is_some_and(|prev| now.duration_since(prev) <= DOUBLE_CTRL_C_MS)
                    {
                        done.set(true);
                        break;
                    }
                    last_ctrl_c = Some(now);
                    editor.borrow_mut().set_text("");
                    client
                        .screen
                        .flash("Press Ctrl+C again to exit".to_string(), None);
                    client.screen.base.request_render(false);
                }
                LauncherKeyEvent::ToggleExpand => {
                    let expanded = view.borrow_mut().toggle_expanded_tools();
                    client.screen.flash(
                        if expanded {
                            "Tool output expanded".to_string()
                        } else {
                            "Tool output collapsed".to_string()
                        },
                        None,
                    );
                    client.screen.base.request_render(true);
                }
                LauncherKeyEvent::ToggleThinking => {
                    let shown = view.borrow_mut().toggle_show_thinking();
                    client.screen.flash(
                        if shown {
                            "Showing thinking blocks".to_string()
                        } else {
                            "Hiding thinking blocks".to_string()
                        },
                        None,
                    );
                    client.screen.base.request_render(true);
                }
                LauncherKeyEvent::Palette => {
                    let entries = build_command_palette_entries(&prefs.keys);
                    let act_sel = panel_actions.clone();
                    let act_cancel = panel_actions.clone();
                    let palette = CommandPalette::new(
                        make_overlay_ctx(&theme),
                        entries,
                        move |id| {
                            push_action(&act_sel, PanelAction::CloseOverlay);
                            push_action(
                                &act_sel,
                                PanelAction::CallContract {
                                    contract: "__palette".into(),
                                    method: id,
                                    args: Value::Null,
                                },
                            );
                        },
                        move || {
                            push_action(&act_cancel, PanelAction::CloseOverlay);
                        },
                    );
                    open_overlay(
                        &mut client,
                        &mut active_overlay,
                        &overlay_open,
                        ComponentRef::new(palette),
                        70,
                    );
                }
                LauncherKeyEvent::OpenResume => {
                    let comp = builtin_panels.open_resume_overlay(&view.borrow(), &theme);
                    open_overlay(
                        &mut client,
                        &mut active_overlay,
                        &overlay_open,
                        ComponentRef::new(BoxedComponent(comp)),
                        75,
                    );
                }
                LauncherKeyEvent::ExternalEditor => {
                    let argv = actions::resolve_editor_argv(prefs.editor_cmd.as_deref());
                    let initial = editor.borrow().text().to_owned();
                    let _ = client.screen.stop(Default::default());
                    let edited = actions::run_external_editor(&initial, &argv);
                    let _ = client.screen.start();
                    match edited {
                        Ok(Some(next_text)) => {
                            editor.borrow_mut().set_text(&next_text);
                        }
                        Ok(None) => {}
                        Err(msg) => client.screen.flash(pi_rs_p2_tui::line_text(&msg), None),
                    }
                    client.screen.base.request_render(true);
                }
                LauncherKeyEvent::Interrupt => {
                    let mut v = view.borrow_mut();
                    let has_run = v.has_active_run();
                    let compacting = v.is_compacting();
                    let shell_id = v.has_live_shell();
                    if has_run || compacting {
                        if compacting {
                            let _ = v.abort_compaction();
                        }
                        if has_run || !compacting {
                            let _ = v.stop_chat();
                        }
                        client.screen.flash("Stopping…".to_string(), None);
                    } else if let Some(sid) = shell_id {
                        let _ = v.abort_shell(&sid);
                        client
                            .screen
                            .flash("Aborted shell command".to_string(), None);
                    } else {
                        let current_text = editor.borrow().text().to_owned();
                        if current_text.trim_start().starts_with('!') {
                            editor.borrow_mut().set_text("");
                        } else if current_text.trim().is_empty() {
                            let now = Instant::now();
                            if last_escape
                                .is_some_and(|prev| now.duration_since(prev) <= DOUBLE_ESCAPE_MS)
                            {
                                last_escape = None;
                                match prefs.double_escape_action() {
                                    "fork" => {
                                        if let Some(comp) =
                                            make_fork_overlay(&v, &theme, &panel_actions)
                                        {
                                            drop(v);
                                            open_overlay(
                                                &mut client,
                                                &mut active_overlay,
                                                &overlay_open,
                                                ComponentRef::new(comp),
                                                75,
                                            );
                                        }
                                    }
                                    "none" => {}
                                    _ => {
                                        let _ = v.request_tree();
                                        if let Some(comp) =
                                            make_tree_overlay(&v, &theme, &panel_actions)
                                        {
                                            drop(v);
                                            open_overlay(
                                                &mut client,
                                                &mut active_overlay,
                                                &overlay_open,
                                                ComponentRef::new(comp),
                                                75,
                                            );
                                        }
                                    }
                                }
                            } else {
                                last_escape = Some(now);
                            }
                        }
                    }
                }
            }
        }
        if done.get() {
            break Ok(());
        }

        // Drain editor submissions & slash commands.
        let submitted = submit.borrow_mut().take();
        if let Some(raw_text) = submitted {
            let trimmed = raw_text.trim();
            if !trimmed.is_empty() {
                editor.borrow_mut().add_to_history(trimmed);
            }
            if let Some(bang_cmd) = trimmed.strip_prefix('!') {
                let cmd = bang_cmd.trim_start();
                let include = !trimmed.starts_with("!!");
                let actual = if include { cmd } else { cmd.trim_start() };
                if let Err(err) = view.borrow_mut().run_shell(actual, include) {
                    client
                        .screen
                        .flash(pi_rs_p2_tui::line_text(&err.to_string()), None);
                }
                editor.borrow_mut().set_text("");
            } else {
                let slash = trimmed.strip_prefix('/').map(|s| {
                    s.split_once(char::is_whitespace)
                        .map_or((s, ""), |(name, args)| (name, args.trim()))
                });
                let mut handled_core = true;
                match slash {
                    Some(("quit", _)) => {
                        done.set(true);
                    }
                    Some(("reload", _)) => {
                        if let Some(p) = &mut plugins {
                            match p.sync_remote().await {
                                Ok(Some(outcome)) => {
                                    *shortcuts.borrow_mut() = p.shortcuts(&prefs.keys);
                                    if outcome.active_removed {
                                        client.close_side_panel();
                                    }
                                    client
                                        .screen
                                        .flash("Reloaded TUI plugins".to_string(), None);
                                }
                                Ok(None) => client
                                    .screen
                                    .flash("TUI plugins up to date".to_string(), None),
                                Err(err) => client
                                    .screen
                                    .flash(pi_rs_p2_tui::line_text(&err.to_string()), None),
                            }
                        } else {
                            prefs = prefs.reload();
                            client.set_theme(&theme, prefs.theme.clone());
                            client.screen.flash("Reloaded TUI".to_string(), None);
                        }
                        editor.borrow_mut().set_text("");
                        client.screen.base.request_render(true);
                    }
                    Some(("hotkeys", _)) => {
                        let entries = build_command_palette_entries(&prefs.keys);
                        let lines = hotkeys_report_text(
                            make_overlay_ctx(&theme).as_ref(),
                            &entries,
                            &prefs.keys,
                            role,
                        );
                        let act_close = panel_actions.clone();
                        let overlay = TextOverlay::new(
                            make_overlay_ctx(&theme),
                            "Keyboard shortcuts",
                            lines,
                            move || push_action(&act_close, PanelAction::CloseOverlay),
                        );
                        open_overlay(
                            &mut client,
                            &mut active_overlay,
                            &overlay_open,
                            ComponentRef::new(overlay),
                            85,
                        );
                        editor.borrow_mut().set_text("");
                    }
                    Some(("settings", _)) => {
                        let mut plugin_fields = indexmap::IndexMap::new();
                        plugin_fields.insert(
                            "quiet-tools.toolCalls".to_string(),
                            quiet_tools_setting_field(),
                        );
                        let act_change = panel_actions.clone();
                        let act_cancel = panel_actions.clone();
                        let raw_settings = prefs.raw_settings.clone();
                        let list = open_settings(
                            make_overlay_ctx(&theme),
                            &raw_settings,
                            plugin_fields,
                            move |path, val| {
                                push_action(
                                    &act_change,
                                    PanelAction::CallContract {
                                        contract: "__save_setting".into(),
                                        method: path.join("."),
                                        args: val.clone(),
                                    },
                                );
                            },
                            move || push_action(&act_cancel, PanelAction::CloseOverlay),
                        );
                        open_overlay(
                            &mut client,
                            &mut active_overlay,
                            &overlay_open,
                            ComponentRef::new(list),
                            75,
                        );
                        editor.borrow_mut().set_text("");
                    }
                    Some(("keybindings", _)) => {
                        let act_change = panel_actions.clone();
                        let act_close = panel_actions.clone();
                        let overlay = KeybindingsOverlay::new(
                            make_overlay_ctx(&theme),
                            prefs.keys.clone(),
                            move |id, val| {
                                push_action(
                                    &act_change,
                                    PanelAction::CallContract {
                                        contract: "__save_key".into(),
                                        method: id.to_owned(),
                                        args: serde_json::to_value(val).unwrap_or(Value::Null),
                                    },
                                );
                            },
                            move || push_action(&act_close, PanelAction::CloseOverlay),
                        );
                        open_overlay(
                            &mut client,
                            &mut active_overlay,
                            &overlay_open,
                            ComponentRef::new(overlay),
                            80,
                        );
                        editor.borrow_mut().set_text("");
                    }
                    Some(("close-panel", _)) => {
                        client.close_side_panel();
                        if let Some(p) = &mut plugins {
                            p.detach();
                        }
                        builtin_panels.close();
                        panel_open.set(false);
                        editor.borrow_mut().set_text("");
                    }
                    _ => handled_core = false,
                }
                if !handled_core {
                    if let Some((id, args)) = slash.and_then(|(name, args)| {
                        let has_dest = view.borrow().destination().is_some();
                        Some((
                            plugins.as_ref()?.slash(
                                name,
                                &command_context(
                                    has_dest,
                                    panel_open.get(),
                                    panel_focus.get(),
                                    false,
                                ),
                            )?,
                            args,
                        ))
                    }) {
                        builtin_panels.close();
                        *command.borrow_mut() = Some((id.to_owned(), args.to_owned()));
                        editor.borrow_mut().set_text("");
                    } else if let Some((_, args)) =
                        slash.filter(|(name, args)| *name == "theme" && !args.is_empty())
                    {
                        match prefs.switch_theme(args, &theme, &mut client) {
                            Ok(applied) => {
                                builtin_panels.set_theme_info(
                                    prefs.current_theme_name().to_owned(),
                                    prefs.available_themes(),
                                );
                                if let Some(frame) = builtin_panels.refresh(&view.borrow(), &theme)
                                {
                                    client.open_side_panel(frame);
                                }
                                client.screen.flash(
                                    pi_rs_p2_tui::line_text(&format!("Theme: {applied}")),
                                    None,
                                );
                            }
                            Err(err) => {
                                client.screen.flash(pi_rs_p2_tui::line_text(&err), None);
                            }
                        }
                        editor.borrow_mut().set_text("");
                        client.screen.base.request_render(true);
                    } else {
                        let outcome = slash.map(|(name, args)| {
                            let mut v = view.borrow_mut();
                            builtin_panels.handle_slash(name, args, &mut v, &theme)
                        });
                        if let Some(outcome) = outcome.filter(|o| o.handled) {
                            if let Some(p) = &mut plugins {
                                p.detach();
                            }
                            if let Some(msg) = outcome.flash {
                                client.screen.flash(msg, None);
                            }
                            if let Some(frame) = outcome.panel {
                                client.open_side_panel(frame);
                                panel_open.set(client.side_panel_shown());
                            }
                            if let Some((_id, comp, width_pct)) = outcome.overlay {
                                open_overlay(
                                    &mut client,
                                    &mut active_overlay,
                                    &overlay_open,
                                    ComponentRef::new(BoxedComponent(comp)),
                                    width_pct,
                                );
                            }
                            editor.borrow_mut().set_text("");
                            client.screen.base.request_render(false);
                        } else {
                            let submitted = view.borrow_mut().submit(&raw_text);
                            if let Err(error) = submitted {
                                break Err(error.into());
                            }
                        }
                    }
                }
            }
        }

        // Drain interactive panel & overlay actions.
        while let Some(action) = panel_actions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
        {
            match action {
                PanelAction::CloseOverlay => {
                    close_overlay(&mut client, &mut active_overlay, &overlay_open, &field);
                }
                PanelAction::Flash(msg) => {
                    client.screen.flash(pi_rs_p2_tui::line_text(&msg), None);
                }
                PanelAction::SwitchSession(id) => {
                    if let Err(e) = view.borrow_mut().switch_session(&id) {
                        client
                            .screen
                            .flash(pi_rs_p2_tui::line_text(&e.to_string()), None);
                    }
                }
                PanelAction::ArchiveSession(id) => {
                    if let Err(e) = view
                        .borrow_mut()
                        .call_session_op("archive", json!({ "session": id }))
                    {
                        client
                            .screen
                            .flash(pi_rs_p2_tui::line_text(&e.to_string()), None);
                    }
                }
                PanelAction::RelocateSession { session, env, cwd } => {
                    let mut payload = json!({ "session": session, "cwd": cwd });
                    if let Some(e) = env.filter(|s| s != "local") {
                        payload["env"] = json!(e);
                    }
                    let _ = view.borrow_mut().call_session_op("relocate", payload);
                }
                PanelAction::ConfigureChat(args) => {
                    let _ = view.borrow_mut().configure_chat(args);
                }
                PanelAction::SetDefaultModel { provider, model_id } => {
                    let mut v = view.borrow_mut();
                    let _ = v.set_default_model(&provider, &model_id);
                    let _ = v.configure_chat(json!({
                        "model": { "provider": provider, "modelId": model_id }
                    }));
                }
                PanelAction::TreeNavigate(entry) => {
                    let _ = view.borrow_mut().navigate_tree(entry);
                }
                PanelAction::SwitchTheme(name) => {
                    match prefs.switch_theme(&name, &theme, &mut client) {
                        Ok(applied) => {
                            builtin_panels.set_theme_info(
                                prefs.current_theme_name().to_owned(),
                                prefs.available_themes(),
                            );
                            if let Some(frame) = builtin_panels.refresh(&view.borrow(), &theme) {
                                client.open_side_panel(frame);
                            }
                            client
                                .screen
                                .flash(pi_rs_p2_tui::line_text(&format!("Theme: {applied}")), None);
                        }
                        Err(e) => client.screen.flash(pi_rs_p2_tui::line_text(&e), None),
                    }
                }
                PanelAction::OpenTextOverlay { title, lines } => {
                    let act_close = panel_actions.clone();
                    let ov = TextOverlay::new(make_overlay_ctx(&theme), title, lines, move || {
                        push_action(&act_close, PanelAction::CloseOverlay);
                    });
                    open_overlay(
                        &mut client,
                        &mut active_overlay,
                        &overlay_open,
                        ComponentRef::new(ov),
                        85,
                    );
                }
                PanelAction::OpenDiffOverlay { title, path, diff } => {
                    let ov = make_diff_overlay(title, path, &diff, &theme, panel_actions.clone());
                    open_overlay(
                        &mut client,
                        &mut active_overlay,
                        &overlay_open,
                        ComponentRef::new(ov),
                        85,
                    );
                }
                PanelAction::OpenBrowser(url_str) => {
                    if let Ok(parsed) = Url::parse(&url_str)
                        && let Err(err) = actions::open_browser(&parsed, None)
                    {
                        client.screen.flash(pi_rs_p2_tui::line_text(&err), None);
                    }
                }
                PanelAction::CopyText(txt) => {
                    clipboard::copy_to_clipboard(&txt, |seq| {
                        client.screen.base.terminal.write(seq);
                    });
                }
                PanelAction::CallContract {
                    contract,
                    method,
                    args,
                } if contract == "__save_setting" => {
                    let path: Vec<String> = method.split('.').map(str::to_owned).collect();
                    match prefs.save_setting_path(&path, &args) {
                        Ok(_) => {
                            let quiet = prefs.quiet_tools_enabled();
                            let pad = prefs.output_pad();
                            view.borrow_mut().configure_display(quiet, pad, None);
                            client
                                .screen
                                .base
                                .set_show_hardware_cursor(prefs.show_hardware_cursor());
                            editor.borrow_mut().set_padding_x(prefs.editor_padding_x());
                            editor
                                .borrow_mut()
                                .set_autocomplete_max_visible(prefs.autocomplete_max_visible());
                            client.screen.base.request_render(true);
                        }
                        Err(e) => client.screen.flash(pi_rs_p2_tui::line_text(&e), None),
                    }
                }
                PanelAction::CallContract {
                    contract,
                    method,
                    args,
                } if contract == "__save_key" => {
                    let val: Option<pi_rs_p2_tui::KeyIdValue> = if args.is_null() {
                        None
                    } else {
                        serde_json::from_value(args).ok()
                    };
                    if let Err(e) = prefs.save_keybinding_override(&method, val.as_ref()) {
                        client.screen.flash(pi_rs_p2_tui::line_text(&e), None);
                    } else {
                        *shortcuts.borrow_mut() = plugins
                            .as_ref()
                            .map(|p| p.shortcuts(&prefs.keys))
                            .unwrap_or_default();
                    }
                }
                PanelAction::CallContract {
                    contract,
                    method,
                    args,
                } if contract == "__palette" => {
                    match method.as_str() {
                        "p2.hotkeys" => *submit.borrow_mut() = Some("/hotkeys".into()),
                        "p2.settings" => *submit.borrow_mut() = Some("/settings".into()),
                        "p2.keybindings" => *submit.borrow_mut() = Some("/keybindings".into()),
                        "app.exit" => done.set(true),
                        "p2.tui.reload" => *submit.borrow_mut() = Some("/reload".into()),
                        "p2.panel.focus" => {
                            if client.side_panel_shown() {
                                client.focus_side_panel(!client.side_panel_focused());
                            }
                        }
                        "p2.panel.close" => *submit.borrow_mut() = Some("/close-panel".into()),
                        "app.tools.expand" => key_queue
                            .borrow_mut()
                            .push_back(LauncherKeyEvent::ToggleExpand),
                        "app.thinking.toggle" => key_queue
                            .borrow_mut()
                            .push_back(LauncherKeyEvent::ToggleThinking),
                        other => {
                            if let Some(slash_name) = other.strip_prefix("slash.") {
                                *submit.borrow_mut() = Some(format!("/{slash_name}"));
                            }
                        }
                    }
                    let _ = args;
                }
                PanelAction::CallContract {
                    contract,
                    method,
                    args,
                } => {
                    if let Err(e) = view.borrow_mut().call_dynamic(&contract, &method, args) {
                        client
                            .screen
                            .flash(pi_rs_p2_tui::line_text(&e.to_string()), None);
                    }
                }
            }
        }

        let requested = command.borrow_mut().take();
        if let Some(p) = &mut plugins {
            match p.poll_reload().await {
                Ok(Some(outcome)) => {
                    *shortcuts.borrow_mut() = p.shortcuts(&prefs.keys);
                    if outcome.active_removed {
                        client.close_side_panel();
                        panel_open.set(false);
                    }
                    for failure in &outcome.failed {
                        client.screen.flash(
                            pi_rs_p2_tui::line_text(&format!(
                                "TUI plugin {} reload failed: {}",
                                failure.plugin, failure.error
                            )),
                            None,
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => client
                    .screen
                    .flash(pi_rs_p2_tui::line_text(&error.to_string()), None),
            }
            if let Some((id, args)) = requested
                && let Err(error) = p.run(&id, &args).await
            {
                client
                    .screen
                    .flash(pi_rs_p2_tui::line_text(&error.to_string()), None);
            }
            match p.panel(&theme) {
                Ok(plugins::PanelUpdate::Show(frame)) => {
                    client.open_side_panel(frame);
                    panel_open.set(true);
                }
                Ok(plugins::PanelUpdate::Close) => {
                    client.close_side_panel();
                    panel_open.set(false);
                }
                Ok(plugins::PanelUpdate::Unchanged) => {}
                Err(error) => client
                    .screen
                    .flash(pi_rs_p2_tui::line_text(&error.to_string()), None),
            }
        }
        panel_focus.set(client.side_panel_focused());
        panel_open.set(client.side_panel_shown());
        chat.set(view.borrow().destination().is_some());

        let stepped = view.borrow_mut().step();
        match stepped {
            Ok(changed) if changed => {
                let mut v = view.borrow_mut();
                if let Some(notice) = v.flash_notice.take() {
                    client.screen.flash(pi_rs_p2_tui::line_text(&notice), None);
                }
                if let Some(draft) = v.restored_draft.take() {
                    editor.borrow_mut().set_text(&draft);
                }
                if destination.as_ref() != v.destination() {
                    if let Some(p) = &mut plugins {
                        p.detach();
                    }
                    builtin_panels.on_session_change();
                    client.close_side_panel();
                    panel_open.set(false);
                    if let Some((old_id, old_conv)) = destination.take() {
                        drafts.insert(
                            format!("{old_id}#{old_conv}"),
                            editor.borrow().text().to_owned(),
                        );
                    }
                    destination = v.destination().cloned();
                    let restored = destination
                        .as_ref()
                        .and_then(|(id, conv)| drafts.remove(&format!("{id}#{conv}")))
                        .unwrap_or_default();
                    editor.borrow_mut().set_text(&restored);
                }
                *cwd_ref.lock().unwrap_or_else(|e| e.into_inner()) = v.active_cwd();
                if let Some(text) = v.accepted.take()
                    && editor.borrow().text() == text
                {
                    editor.borrow_mut().set_text("");
                }
                if let Some(frame) = builtin_panels.refresh(&v, &theme) {
                    client.open_side_panel(frame);
                }
                grants.borrow_mut().display(&v.last_message);
                *footer_left.borrow_mut() = v.title().to_owned();
                if let (Some(prov), Some(mid)) = (
                    v.chat_model
                        .as_ref()
                        .and_then(|m| m["provider"].as_str())
                        .or_else(|| v.default_model["provider"].as_str()),
                    v.chat_model
                        .as_ref()
                        .and_then(|m| m["modelId"].as_str())
                        .or_else(|| v.default_model["modelId"].as_str()),
                ) {
                    *footer_right.borrow_mut() = format!("({prov}) {mid}");
                }
                client.screen.base.request_render(false);
            }
            Ok(_) => {}
            Err(e) => break Err(Box::<dyn Error>::from(e)),
        }
        if let Err(e) = client.step(Duration::from_millis(50)) {
            break Err(e.into());
        }
        if theme.version() != last_theme_version {
            last_theme_version = theme.version();
            if let Some(frame) = builtin_panels.refresh(&view.borrow(), &theme) {
                client.open_side_panel(frame);
            }
            client.screen.base.request_render(true);
        }
    };
    let stop_seq = color_probe.borrow_mut().stop_sequence();
    client.screen.base.terminal.write(stop_seq);
    client.screen.stop(Default::default())?;
    if let Some(p) = plugins {
        p.close().await?;
    }
    result
}

fn command_context(chat: bool, panel: bool, focused: bool, empty: bool) -> Vec<&'static str> {
    let mut active = vec!["connected", "owner", if chat { "chat" } else { "sessions" }];
    if panel {
        active.push("panel");
    }
    if focused {
        active.push("panelFocused");
    }
    if empty {
        active.push("editorEmpty");
    }
    active
}

async fn local_sign_in(
    client: &Client,
    url: &Url,
    home: &std::path::Path,
) -> Result<String, Box<dyn Error>> {
    let origin = url.origin().ascii_serialization();
    let dir = env::var_os("P2_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".pi/p2"));
    let key = std::fs::read_to_string(dir.join("owner-key"))?;
    let post = |path| -> Result<_, Box<dyn Error>> {
        Ok(client.post(url.join(path)?).header(header::ORIGIN, &origin))
    };
    let value = post(LOCAL_CHALLENGE_PATH)?
        .json(&json!({}))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    let challenge = value["challenge"]
        .as_str()
        .ok_or("Invalid owner challenge")?;
    let proof = local_owner_proof(
        key.trim(),
        url.port_or_known_default().ok_or("Missing port")?,
        challenge,
    );
    let response = post(LOCAL_CLAIM_PATH)?
        .json(&json!({ "challenge": challenge, "proof": proof }))
        .send()
        .await?
        .error_for_status()?;
    response_cookie(&response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cli_args_supports_radius_and_rejects_mixed_sources() -> Result<(), Box<dyn Error>> {
        let parsed = parse_cli_args(["--radius"].into_iter().map(str::to_owned))?;
        assert_eq!(parsed.radius_server, Some(None));

        let id = "12345678-1234-1234-1234-1234567890ab";
        let with_id = parse_cli_args(["--radius", id].into_iter().map(str::to_owned))?;
        assert_eq!(with_id.radius_server, Some(Some(id.to_owned())));

        let bad = parse_cli_args(
            ["--radius", "http://127.0.0.1:8788"]
                .into_iter()
                .map(str::to_owned),
        );
        assert!(bad.is_err());
        Ok(())
    }
}

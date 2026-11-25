mod admission;
mod builtin_panels;
mod grants;
pub mod plugins;
mod preferences;
#[cfg(test)]
mod pty;
mod view;
mod when;
use crate::runtime::local_owner::*;
use pi_rs_chord::node::http::connect_websocket;
use pi_rs_p2_tui::{
    RenderContext,
    components::{BuiltInHeader, HeaderSource, spread},
    host::TuiClient,
};
use pi_rs_tui::{
    Component, ComponentRef, Focusable, Input, ProcessTerminal, Tui, TuiInputListenerResult,
};
use reqwest::{Client, Url, header};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    env,
    error::Error,
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

pub async fn run() -> Result<(), Box<dyn Error>> {
    let default_address = format!(
        "http://127.0.0.1:{}",
        env::var("PORT").unwrap_or_else(|_| "8788".into())
    );
    let mut theme = None;
    let mut no_themes = false;
    let mut address = None;
    let mut join_link = None;
    let mut explicit_owner = false;
    let mut owner_link = env::var("P2_TUI_OWNER_LINK").ok().filter(|s| !s.is_empty());
    let mut args = env::args_os()
        .skip(2)
        .map(|arg| arg.to_string_lossy().into_owned());
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
        if arg.starts_with('-') {
            return Err(format!("Unknown TUI option: {arg}").into());
        }
        address = Some(arg);
    }
    if join_link.is_some() && explicit_owner {
        return Err("Use either --join or --owner-link".into());
    }
    let address = address
        .or_else(|| {
            join_link
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
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?;
    let home = env::home_dir().ok_or("Cannot determine home directory")?;
    let config = env::var_os("P2_TUI_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".pi/p2-tui"))
        .join("config.json");
    let config_path = config;
    let mut config = admission::load(&config_path)?;
    let mut guest = false;
    let mut local_admission = false;
    let mut preferred = None;
    let mut cookie = None;
    let explicit = match (join_link, owner_link) {
        (Some(link), _) => Some((link, true)),
        (None, Some(link)) => Some((
            Url::parse(link.trim()).map_err(|_| "Invalid owner link")?,
            false,
        )),
        _ => None,
    };
    if let Some((link, is_guest)) = explicit {
        guest = is_guest;
        let (admitted, session) =
            admission::redeem(&client, &url, &link, &mut config, &config_path, guest).await?;
        cookie = Some(admitted);
        preferred = session;
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
            cookie = Some(saved.to_owned());
        } else if !matches!(check.status().as_u16(), 401 | 403) {
            return Err("Cannot check owner access".into());
        }
    }
    if cookie.is_none()
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
            cookie = Some(saved.to_owned());
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
        } else {
            return Err("Cannot check guest access".into());
        }
    }
    let mut cookie = match cookie {
        Some(cookie) => cookie,
        None => {
            let local_cookie = if local {
                match local_sign_in(&client, &url, &home).await {
                    Ok(cookie) => Some(cookie),
                    Err(error) => {
                        eprintln!("Local sign-in unavailable: {error}");
                        None
                    }
                }
            } else {
                None
            };
            if let Some(cookie) = local_cookie {
                local_admission = true;
                cookie
            } else {
                eprint!("Owner or share link: ");
                let mut input = String::new();
                if std::io::stdin().read_line(&mut input)? == 0 {
                    return Err("Sign-in requires an owner or share link".into());
                }
                let link = Url::parse(input.trim()).map_err(|_| "Invalid sign-in link")?;
                guest = link.path().starts_with("/join/");
                let (cookie, session) = admission::redeem(
                    &client,
                    &url,
                    &link,
                    &mut config,
                    &config_path,
                    link.path().starts_with("/join/"),
                )
                .await?;
                preferred = session;
                cookie
            }
        }
    };
    let mut socket = url.join("/chord?client=tui")?;
    socket
        .set_scheme(if local { "ws" } else { "wss" })
        .map_err(|_| "Invalid socket origin")?;
    loop {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::ORIGIN, origin.parse()?);
        let mut credential = header::HeaderValue::from_str(&cookie)?;
        credential.set_sensitive(true);
        headers.insert(header::COOKIE, credential);
        let socket_open = tokio::time::timeout(
            Duration::from_secs(10),
            connect_websocket(socket.as_str(), headers, 4 * 1024 * 1024, 4 * 1024 * 1024),
        );
        let result = match socket_open.await? {
            Ok(link) => {
                let prefs = preferences::Preferences::load(
                    &home,
                    config_path.parent().ok_or("Missing config directory")?,
                    theme.clone(),
                    no_themes,
                );
                let local_fallback = (local && !guest).then(|| {
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
                    poll_interval: Duration::from_secs(1),
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

struct HeaderRenderCtx(pi_rs_p2_tui::ThemeRef);

impl RenderContext for HeaderRenderCtx {
    fn theme(&self) -> &pi_rs_p2_tui::ThemeRef {
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
    theme: pi_rs_p2_tui::ThemeRef,
}

impl Component for TranscriptPane {
    fn render(&self, width: usize) -> Vec<String> {
        self.view
            .borrow()
            .render_transcript_lines(&self.theme, width)
    }
}

struct EditorDock {
    editor: Rc<RefCell<Input>>,
    theme: pi_rs_p2_tui::ThemeRef,
    footer_left: Rc<RefCell<String>>,
    footer_right: Rc<RefCell<String>>,
}

impl Component for EditorDock {
    fn render(&self, width: usize) -> Vec<String> {
        let width = width.max(1);
        let theme = self.theme.current();
        let rule = theme.fg("borderMuted", &"─".repeat(width));
        let mut lines = Vec::new();
        lines.push(rule.clone());
        for line in self.editor.borrow().render(width) {
            lines.push(pi_rs_tui::truncate_to_width(&line, width, false));
        }
        lines.push(rule);
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

async fn mount(
    view: view::View,
    prefs: preferences::Preferences,
    mut plugins: Option<plugins::Plugins>,
    origin: String,
    role: &str,
) -> Result<(), Box<dyn Error>> {
    let mode = prefs.theme.color_mode;
    let terminal = prefs.theme.terminal.clone();
    let theme = pi_rs_p2_tui::ThemeRef::new(prefs.theme);

    let hint_theme = theme.current();
    let header_component = BuiltInHeader::new(HeaderSource {
        ctx: Box::new(HeaderRenderCtx(theme.clone())),
        origin: Some(origin),
        role: Some(role.to_owned()),
        connected: true,
        expanded: false,
        hints: Box::new(move || {
            vec![
                format!(
                    "{} {}",
                    hint_theme.fg("dim", "escape"),
                    hint_theme.fg("muted", "interrupt")
                ),
                format!(
                    "{} {}",
                    hint_theme.fg("dim", "ctrl+c"),
                    hint_theme.fg("muted", "quit")
                ),
                format!(
                    "{} {}",
                    hint_theme.fg("dim", "/"),
                    hint_theme.fg("muted", "commands")
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

    let view = Rc::new(RefCell::new(view));
    let root = ComponentRef::new(TranscriptPane {
        view: view.clone(),
        theme: theme.clone(),
    });
    let submit = Rc::new(RefCell::new(None));
    let posted = submit.clone();
    let mut editor = Input::default();
    editor.on_submit = Some(Box::new(move |text| {
        *posted.borrow_mut() = Some(text.to_owned())
    }));
    let editor = Rc::new(RefCell::new(editor));
    let footer_left = Rc::new(RefCell::new(view.borrow().title().to_owned()));
    let footer_right = Rc::new(RefCell::new("p2 tui".to_owned()));
    let field = ComponentRef::new(EditorDock {
        editor: editor.clone(),
        theme: theme.clone(),
        footer_left: footer_left.clone(),
        footer_right: footer_right.clone(),
    });
    let mut client =
        TuiClient::mount_panel(ProcessTerminal::new(), header, root, field, prefs.mouse);
    if let Some(path) = prefs.theme_path
        && let Err(error) = client.watch_theme(&path, theme.clone(), mode, terminal)
    {
        eprintln!("Live theme reload unavailable: {error}");
    }
    let done = Rc::new(Cell::new(false));
    let quit = done.clone();
    let grants = Rc::new(std::cell::RefCell::new(grants::CopyGrant::default()));
    let mut copy_key = grants::CopyGrant::listener(grants.clone(), &prefs.keys, editor.clone());
    let picker_key = Rc::new(Cell::new(None));
    let selection = picker_key.clone();
    let visible = Rc::new(Cell::new(false));
    let picking = visible.clone();
    let mut drafts = std::collections::HashMap::new();
    let mut destination = None;
    let mut builtin_panels = builtin_panels::BuiltinPanels::default();
    let shortcuts = Rc::new(std::cell::RefCell::new(
        plugins
            .as_ref()
            .map(|p| p.shortcuts(&prefs.keys))
            .unwrap_or_default(),
    ));
    let shortcut_table = shortcuts.clone();
    let command = Rc::new(std::cell::RefCell::new(None));
    let shortcut_command = command.clone();
    let panel_open = Rc::new(Cell::new(false));
    let panel_focus = Rc::new(Cell::new(false));
    let open = panel_open.clone();
    let focused = panel_focus.clone();
    let focus_change = Rc::new(Cell::new(None));
    let change = focus_change.clone();
    let chat = Rc::new(Cell::new(false));
    let session = chat.clone();
    let input = editor.clone();
    client.screen.base.add_input_listener(Box::new(move |data| {
        let quit_key = pi_rs_tui::keys::matches_key_data(data, "ctrl+c");
        let matches = |key| pi_rs_tui::keys::matches_key_data(data, key);
        let pick = if matches("ctrl+r") {
            Some("open")
        } else if picking.get() {
            ["up", "down", "enter", "escape"]
                .into_iter()
                .find(|key| matches(key))
        } else {
            None
        };
        if let Some(key) = pick {
            selection.set(Some(key));
        }
        let mut consume = quit_key || pick.is_some() || picking.get() || copy_key(data);
        if !consume && open.get() && (matches("f2") || (focused.get() && matches("escape"))) {
            change.set(Some(!focused.get()));
            consume = true;
        }
        if !consume
            && let Some((_, id, _)) = shortcut_table.borrow().iter().find(|(key, _, when)| {
                matches(key)
                    && when.matches(&command_context(
                        session.get(),
                        open.get(),
                        focused.get(),
                        input.borrow().value().is_empty(),
                    ))
            })
        {
            *shortcut_command.borrow_mut() = Some((id.clone(), String::new()));
            consume = true;
        }
        if quit_key {
            quit.set(true);
        }
        TuiInputListenerResult {
            consume,
            data: None,
        }
    }));
    client.screen.start()?;
    let result = loop {
        if let Some(write) = grants.borrow_mut().take() {
            client.screen.base.terminal.write(&write);
        }
        if done.get() {
            break Ok(());
        }
        if let Some(focus) = focus_change.take() {
            client.focus_side_panel(focus);
        }
        if let Some(text) = submit.borrow_mut().take() {
            let slash = text.trim().strip_prefix('/').map(|s| {
                s.split_once(char::is_whitespace)
                    .map_or((s, ""), |(name, args)| (name, args.trim()))
            });
            if slash == Some(("close-panel", "")) && client.close_side_panel() {
                if let Some(p) = &mut plugins {
                    p.detach();
                }
                builtin_panels.close();
                editor.borrow_mut().set_value(String::new());
                panel_open.set(false);
            } else if let Some((id, args)) = slash.and_then(|(name, args)| {
                let has_dest = view.borrow().destination().is_some();
                Some((
                    plugins.as_ref()?.slash(
                        name,
                        &command_context(has_dest, panel_open.get(), panel_focus.get(), false),
                    )?,
                    args,
                ))
            }) {
                builtin_panels.close();
                *command.borrow_mut() = Some((id.to_owned(), args.to_owned()));
                editor.borrow_mut().set_value(String::new());
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
                    editor.borrow_mut().set_value(String::new());
                    client.screen.base.request_render(false);
                } else {
                    let submitted = view.borrow_mut().submit(&text);
                    if let Err(error) = submitted {
                        break Err(error.into());
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
        let mut selection_changed = false;
        if let Some(key) = picker_key.take() {
            let chosen = view.borrow_mut().choose(key);
            if let Err(error) = chosen {
                break Err(error.into());
            }
            selection_changed = true;
            visible.set(view.borrow().choosing());
        }
        let stepped = view.borrow_mut().step();
        match stepped {
            Ok(changed) if changed || selection_changed => {
                let mut v = view.borrow_mut();
                if destination.as_ref() != v.destination() {
                    if let Some(p) = &mut plugins {
                        p.detach();
                    }
                    builtin_panels.on_session_change();
                    client.close_side_panel();
                    panel_open.set(false);
                    if let Some(old) = destination.take() {
                        drafts.insert(old, editor.borrow().value().to_owned());
                    }
                    destination = v.destination().cloned();
                    let restored = destination
                        .as_ref()
                        .and_then(|key| drafts.remove(key))
                        .unwrap_or_default();
                    editor.borrow_mut().set_value(restored);
                }
                if let Some(text) = v.accepted.take()
                    && editor.borrow().value() == text
                {
                    editor.borrow_mut().set_value(String::new());
                }
                if let Some(frame) = builtin_panels.refresh(&v, &theme) {
                    client.open_side_panel(frame);
                }
                grants.borrow_mut().display(&v.last_message);
                *footer_left.borrow_mut() = v.title().to_owned();
                if let (Some(prov), Some(mid)) = (
                    v.default_model["provider"].as_str(),
                    v.default_model["modelId"].as_str(),
                ) {
                    *footer_right.borrow_mut() = format!("({prov}) {mid}");
                }
                client.screen.base.request_render(false);
            }
            Ok(_) => {}
            Err(e) => break Err(Box::<dyn Error>::from(e)),
        }
        if let Err(e) = client.step(Duration::from_millis(100)) {
            break Err(e.into());
        }
    };
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
        .json(&json!({ "challenge":challenge,"proof":proof }))
        .send()
        .await?
        .error_for_status()?;
    response_cookie(&response)
}

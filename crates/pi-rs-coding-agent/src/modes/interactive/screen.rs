use std::{
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use pi_rs_tui::{
    ComponentRef, ProcessTerminal, ProgramStatus, ScrollView, ScrollViewOptions, Scrollbar,
    StackEntryOptions, StackOptions, Terminal, Tui, TuiAltScreen, TuiAltScreenOptions,
    TuiMainScreen, TuiMainScreenRenderState, TuiMode, TuiStopOptions, VStack, ViewportTui,
    WheelScrollLines, layout_node::StackBasis, terminal::TerminalEvent,
};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::utils::{clipboard, open_browser};

use super::{UiEvent, theme::Theme};

// Screen switches retain the same terminal transport. Only the renderer owns input;
// the control handle is used for the terminal-specific status API.
struct Transport<T>(Arc<Mutex<T>>);

impl<T> Transport<T> {
    fn with<R>(&self, op: impl FnOnce(&mut T) -> R) -> R {
        op(&mut self.0.lock().unwrap_or_else(|error| error.into_inner()))
    }
}

impl<T: Terminal> Terminal for Transport<T> {
    fn width(&self) -> usize {
        self.with(|terminal| terminal.width())
    }
    fn height(&self) -> usize {
        self.with(|terminal| terminal.height())
    }
    fn write(&mut self, data: &str) {
        self.with(|terminal| terminal.write(data));
    }
    fn read_available(&mut self) -> Vec<u8> {
        self.with(Terminal::read_available)
    }
    fn is_tty(&self) -> bool {
        self.with(|terminal| terminal.is_tty())
    }
    fn start_session(&mut self) -> io::Result<()> {
        self.with(Terminal::start_session)
    }
    fn stop_session(&mut self) -> io::Result<()> {
        self.with(Terminal::stop_session)
    }
    fn set_exit_restore(&mut self, before: &str, after: &str) {
        self.with(|terminal| terminal.set_exit_restore(before, after));
    }
    fn poll_input(&mut self, timeout: Duration) -> io::Result<Vec<TerminalEvent>> {
        self.with(|terminal| terminal.poll_input(timeout))
    }
}

pub(super) struct TerminalHandle {
    create: Box<dyn Fn() -> Box<dyn Terminal>>,
    pub(super) report: Option<Box<dyn FnMut(ProgramStatus)>>,
}

impl TerminalHandle {
    pub(super) fn injected(terminal: impl Terminal + 'static) -> Self {
        Self::make(Arc::new(Mutex::new(terminal)))
    }

    pub(super) fn process() -> Self {
        let process = Arc::new(Mutex::new(ProcessTerminal::new()));
        let mut handle = Self::make(process.clone());
        handle.report = Some(Box::new(move |status| {
            Transport(process.clone()).with(|terminal| terminal.set_program_status(status));
        }));
        handle
    }

    fn make<T: Terminal + 'static>(terminal: Arc<Mutex<T>>) -> Self {
        Self {
            create: Box::new(move || Box::new(Transport(terminal.clone()))),
            report: None,
        }
    }
}

struct OwnedTerminal(Box<dyn Terminal>);

impl Terminal for OwnedTerminal {
    fn width(&self) -> usize {
        self.0.width()
    }
    fn height(&self) -> usize {
        self.0.height()
    }
    fn write(&mut self, data: &str) {
        self.0.write(data);
    }
    fn read_available(&mut self) -> Vec<u8> {
        self.0.read_available()
    }
    fn is_tty(&self) -> bool {
        self.0.is_tty()
    }
    fn start_session(&mut self) -> io::Result<()> {
        self.0.start_session()
    }
    fn stop_session(&mut self) -> io::Result<()> {
        self.0.stop_session()
    }
    fn set_exit_restore(&mut self, before: &str, after: &str) {
        self.0.set_exit_restore(before, after);
    }
    fn poll_input(&mut self, timeout: Duration) -> io::Result<Vec<TerminalEvent>> {
        self.0.poll_input(timeout)
    }
}

pub(super) enum Screen {
    Main(Box<TuiMainScreen>),
    Alt(
        Box<TuiAltScreen>,
        Option<std::rc::Rc<pi_rs_tui::components::scroll_view::ScrollState>>,
    ),
}

impl Screen {
    pub(super) fn new(
        terminal: &TerminalHandle,
        mode: TuiMode,
        agent_dir: PathBuf,
        settings: &Value,
        theme: &Theme,
        events: &UnboundedSender<UiEvent>,
    ) -> Self {
        let terminal = OwnedTerminal((terminal.create)());
        let hardware_cursor = crate::core::settings_manager::show_hardware_cursor_from(settings);
        let mut screen = match mode {
            TuiMode::Regular => Self::Main(Box::new(TuiMainScreen::new(
                terminal,
                hardware_cursor,
                Some(agent_dir),
            ))),
            TuiMode::Fullscreen => {
                let color = theme.clone();
                let current = theme.clone();
                let end = theme.clone();
                let paste = events.clone();
                Self::Alt(
                    Box::new(TuiAltScreen::new(
                        terminal,
                        hardware_cursor,
                        TuiAltScreenOptions {
                            wheel_scroll_lines: settings["fullscreenWheelScrollLines"]
                                .as_f64()
                                .map_or(WheelScrollLines::Auto, WheelScrollLines::Fixed),
                            copy_on_select: settings["fullscreenCopyOnSelect"]
                                .as_bool()
                                .unwrap_or(true),
                            copy_selection: Some(Box::new(|text| copy(text).map_err(Some))),
                            open_url: Some(Box::new(open_browser::open_browser)),
                            on_right_click_paste: Some(Box::new(move || {
                                let _ = paste.send(UiEvent::Paste);
                            })),
                            search_match_style: Some(std::rc::Rc::new(move |s| {
                                format!(
                                    "\x1b[4m{}\x1b[24m",
                                    color.bg("searchMatchBg", &color.fg("searchMatchText", s))
                                )
                            })),
                            search_current_match_style: Some(std::rc::Rc::new(move |s| {
                                format!(
                                    "\x1b[1;7m{}\x1b[22;27m",
                                    current.bg("searchMatchBg", &current.fg("searchMatchText", s))
                                )
                            })),
                            scroll_to_end_indicator: Some(Box::new(move || {
                                end.bg("selectedBg", " ↓ Jump to latest message ")
                            })),
                            ..Default::default()
                        },
                    )),
                    None,
                )
            }
        };
        let base = screen.tui_mut().base_mut();
        base.set_clear_on_shrink(
            settings["terminal"]["clearOnShrink"]
                .as_bool()
                .unwrap_or(false),
        );
        let scheme = events.clone();
        base.on_terminal_color_scheme_change(Box::new(move |scheme_value| {
            let _ = scheme.send(UiEvent::ColorScheme(scheme_value));
        }));
        #[cfg(unix)]
        {
            let suspend = events.clone();
            // Before start this only stores the callback; the renderer registers the signal handler on start.
            let _ = base.set_suspend_signal_handler(Some(Box::new(move || {
                let _ = suspend.send(UiEvent::Suspend);
            })));
        }
        screen
    }

    pub(super) fn mount(
        &mut self,
        transcript: ComponentRef,
        dock: [ComponentRef; 6],
        theme: &Theme,
        settings: &Value,
    ) {
        match self {
            Self::Main(main) => {
                main.base.content.add_child(transcript);
                for child in dock {
                    main.base.content.add_child(child);
                }
            }
            Self::Alt(alt, scroll_state) => {
                let track = theme.clone();
                let thumb = theme.clone();
                let scroll = ScrollView::new(
                    transcript,
                    ScrollViewOptions {
                        follow_end: true,
                        primary: true,
                        scrollbar: scrollbar(settings),
                        scrollbar_track_style: Some(std::rc::Rc::new(move |text| {
                            track.fg("scrollbarTrack", text)
                        })),
                        scrollbar_thumb_style: Some(std::rc::Rc::new(move |text| {
                            thumb.fg("scrollbarThumb", text)
                        })),
                        ..Default::default()
                    },
                );
                *scroll_state = Some(scroll.state.clone());
                let scroll = ComponentRef::new(scroll);
                let dock = ComponentRef::new(VStack::new(
                    dock.into_iter().enumerate().map(|(index, child)| {
                        (
                            child,
                            StackEntryOptions {
                                min_size: Some(if index == 3 { 3 } else { 0 }),
                                ..Default::default()
                            },
                        )
                    }),
                    StackOptions::default(),
                ));
                let root = ComponentRef::new(VStack::new(
                    [
                        (
                            scroll,
                            StackEntryOptions {
                                basis: StackBasis::Cells(0),
                                grow: 1.,
                                min_size: Some(1),
                                ..Default::default()
                            },
                        ),
                        (
                            dock,
                            StackEntryOptions {
                                min_size: Some(1),
                                ..Default::default()
                            },
                        ),
                    ],
                    StackOptions::default(),
                ));
                alt.set_layout_root(Some(root));
            }
        }
    }

    pub(super) fn settings_changed(&mut self, settings: &Value) {
        if let Self::Alt(alt, scroll) = self {
            alt.set_wheel_scroll_lines(
                settings["fullscreenWheelScrollLines"]
                    .as_f64()
                    .map_or(WheelScrollLines::Auto, WheelScrollLines::Fixed),
            );
            alt.set_copy_on_select(settings["fullscreenCopyOnSelect"].as_bool().unwrap_or(true));
            if let Some(scroll) = scroll {
                scroll.set_scrollbar(scrollbar(settings));
            }
        }
    }

    pub(super) fn tui_mut(&mut self) -> &mut dyn Tui {
        match self {
            Self::Main(tui) => tui.as_mut(),
            Self::Alt(tui, _) => tui.as_mut(),
        }
    }

    pub(super) fn step(&mut self) -> io::Result<()> {
        match self {
            Self::Main(tui) => tui.step(Duration::ZERO),
            Self::Alt(tui, _) => tui.step(Duration::ZERO),
        }
    }

    pub(super) fn fullscreen_frame(&self) -> Option<&[String]> {
        match self {
            Self::Main(_) => None,
            Self::Alt(tui, _) => Some(tui.screen_lines()),
        }
    }

    pub(super) fn capture_main(&self) -> Option<TuiMainScreenRenderState> {
        match self {
            Self::Main(tui) => Some(tui.capture_render_state()),
            Self::Alt(..) => None,
        }
    }

    pub(super) fn restore_main(&mut self, state: TuiMainScreenRenderState) {
        if let Self::Main(tui) = self {
            tui.restore_render_state(state);
        }
    }

    pub(super) fn stop(&mut self, preserve_screen: bool) -> io::Result<()> {
        self.tui_mut().stop(TuiStopOptions { preserve_screen })
    }
}

fn scrollbar(settings: &Value) -> Scrollbar {
    match settings["fullscreenScrollbar"].as_str() {
        Some("always") => Scrollbar::Always,
        Some("off" | "hidden") => Scrollbar::Hidden,
        _ => Scrollbar::Auto,
    }
}

pub(super) fn copy(text: &str) -> Result<(), String> {
    clipboard::copy_text(
        text,
        clipboard::Platform::current(),
        &clipboard::ClipboardEnv::from_env(),
    )
}

pub(super) fn mode(settings: &Value, override_mode: Option<TuiMode>) -> TuiMode {
    override_mode.unwrap_or_else(|| match settings["tuiMode"].as_str() {
        Some("regular" | "main") => TuiMode::Regular,
        _ => TuiMode::Fullscreen,
    })
}

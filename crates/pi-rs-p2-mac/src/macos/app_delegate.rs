use super::{
    MacPreferences,
    brain_page::{Load, Page, json_value},
    main_menu, open_url,
    window_controller::Window,
};
use crate::{
    Error, Result,
    appearance::{self, Appearance},
    brain::{Brain, Route},
    brain_directory::{BrainDirectory, POLL_INTERVAL},
    dev_control,
    diagnostics::{BridgeCapture, Diagnostics, PageCapture},
    host_bridge,
    local_sign_in::{self, WebCookie},
    pi_glyph,
    radius_account::RadiusAccount,
    server_config::{
        Bundled, Environment, Launch, Preferences, ServerConfig, Source, environment, p2_directory,
    },
    server_controller::{Ownership, PROBE_INTERVAL, Phase, ServerController, State},
    server_scan::{self, FoundServer},
    updater, view_cookie,
};
use block2::RcBlock;
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Retained, Weak},
    runtime::{AnyObject, NSObjectProtocol, ProtocolObject, Sel},
    sel,
};
use objc2_app_kit::{
    NSAlert, NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    NSApplicationActivationPolicy, NSApplicationDelegate, NSApplicationTerminateReply,
    NSBezierPath, NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSImage, NSMenu, NSMenuDelegate, NSMenuItem, NSModalResponseOK,
    NSOpenPanel, NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSView, NSWorkspace,
};
use objc2_foundation::{
    NSBundle, NSDictionary, NSNotification, NSObject, NSPoint, NSProcessInfo, NSRect, NSSize,
    NSString, NSTimer, NSURL,
};
use objc2_quartz_core::CAMediaTiming;
use objc2_web_kit::{WKContentWorld, WKWebView};
use std::{
    cell::RefCell,
    io::Write,
    path::{Path, PathBuf},
    ptr::NonNull,
    rc::Rc,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use tokio::{
    runtime::Runtime,
    sync::{Mutex, oneshot},
    task::JoinHandle,
};
use url::Url;

struct Models {
    config: ServerConfig,
    radius: RadiusAccount,
    server: Arc<Mutex<ServerController>>,
    tasks: Vec<JoinHandle<()>>,
}
impl Drop for Models {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

pub(super) struct StateUi {
    runtime: Runtime,
    events: mpsc::Receiver<Event>,
    send: mpsc::Sender<Event>,
    generation: u64,
    env: Environment,
    launch: Launch,
    bundled: Option<Bundled>,
    build: String,
    appearance: Appearance,
    model: Models,
    phase: State,
    brains: Vec<Brain>,
    shows_tabs: bool,
    windows: Vec<Window>,
    found: Vec<FoundServer>,
    next_id: u64,
    status: Option<Retained<NSStatusItem>>,
    timer: Option<Retained<NSTimer>>,
    diagnostics: Option<Diagnostics>,
    changing: bool,
    quitting: bool,
}

enum Event {
    Phase(u64, State),
    Directory(u64, Vec<Brain>, bool),
    Servers(Vec<FoundServer>),
    Loaded(u64, Result<Load>),
    Renewed(u64, WebCookie),
    Prompt(String),
    OpenUrl(Url),
    Install(PathBuf),
    Switch(ServerConfig),
    Quit,
    QuitReady,
    Diagnose(u64, usize),
    OpenBrain(String, String),
}

define_class!(
    // SAFETY: main-thread AppKit subclass; its async workers exchange owned Rust data, never Cocoa objects.
    #[unsafe(super(NSObject))]
    #[thread_kind=objc2::MainThreadOnly]
    #[ivars=RefCell<StateUi>]
    pub(super) struct AppDelegate;
    // SAFETY: inherits NSObject protocol selectors unchanged.
    unsafe impl NSObjectProtocol for AppDelegate {}
    // SAFETY: all declared selectors use the concrete types and matching AppKit return conventions.
    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn launched(&self,_notification:&NSNotification) { self.setup(); }
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn reopen(&self,_:&NSApplication,visible:bool)->bool { if !visible {self.show();} true }
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn last_window(&self,_:&NSApplication)->bool { false }
        #[unsafe(method(applicationShouldTerminate:))]
        fn terminate(&self,_:&NSApplication)->NSApplicationTerminateReply {
            let mut ui=self.ivars().borrow_mut();
            if ui.quitting {return NSApplicationTerminateReply::TerminateLater;}
            ui.quitting=true;
            let server=ui.model.server.clone(); let out=ui.send.clone();
            ui.runtime.spawn(async move {let _=server.lock().await.stop().await;let _=out.send(Event::QuitReady);});
            NSApplicationTerminateReply::TerminateLater
        }
    }
    // SAFETY: the status menu is retained by AppKit and only calls back on the main thread.
    unsafe impl NSMenuDelegate for AppDelegate {
        #[unsafe(method(menuWillOpen:))]
        fn menu_open(&self,_:&NSMenu){self.scan();}
        #[unsafe(method(menuNeedsUpdate:))]
        fn needs_update(&self,menu:&NSMenu){self.fill_status(menu);}
    }
    impl AppDelegate {
        #[unsafe(method(openPi:))]
        fn open_pi(&self,_:&AnyObject){self.show();}
        #[unsafe(method(newWindow:))]
        fn new_window(&self,_:&AnyObject){if let Some(page)=self.key_page(){self.open_brain(&page.brain.id,&page.current_path());}else{self.show();}}
        #[unsafe(method(appearance:))]
        fn choose_appearance(&self,item:&NSMenuItem){if let Ok(i)=usize::try_from(item.tag()) && let Some(&next)=Appearance::ALL.get(i){let _=self.set_appearance(next);}}
        #[unsafe(method(startServer:))]
        fn start(&self,_:&AnyObject){self.start_server();}
        #[unsafe(method(stopServer:))]
        fn stop(&self,_:&AnyObject){self.server_action(false);}
        #[unsafe(method(restartServer:))]
        fn restart(&self,_:&AnyObject){self.server_action(true);}
        #[unsafe(method(useFound:))]
        fn use_found(&self,item:&NSMenuItem){
            let config={let ui=self.ivars().borrow();usize::try_from(item.tag()).ok().and_then(|i|ui.found.get(i)).map(|f|ui.launch.own.attached(f.port,f.data_dir.clone()))};
            if let Some(config)=config{self.switch(config);}
        }
        #[unsafe(method(useOwn:))]
        fn use_own(&self,_:&AnyObject){let own=self.ivars().borrow().launch.own.clone();self.switch(own);}
        #[unsafe(method(chooseCheckout:))]
        fn checkout(&self,_:&AnyObject){
            let panel=NSOpenPanel::openPanel(self.mtm());
            panel.setCanChooseDirectories(true);panel.setCanChooseFiles(false);panel.setMessage(Some(&NSString::from_str("Choose a pi-mono checkout or its apps/p2 directory")));
            if panel.runModal()!=NSModalResponseOK{return;}
            let selected=panel.URL().and_then(|u|u.path()).map(|s|PathBuf::from(s.to_string())).and_then(|p|p2_directory(&p));
            if let Some(selected)=selected {
                if let Err(error)=MacPreferences.set("checkout",selected.to_str()){self.alert("Could not save checkout",&error.to_string());return;}
                self.refresh_own();
            } else {self.alert("No p2 in that directory","Choose a folder containing apps/p2/src/hosts/server.ts.");}
        }
        #[unsafe(method(forgetCheckout:))]
        fn forget_checkout(&self,_:&AnyObject){let _=MacPreferences.set("checkout",None);self.refresh_own();}
        #[unsafe(method(showLog:))]
        fn show_log(&self,_:&AnyObject){
            let ui=self.ivars().borrow();let path=ui.launch.home.join(format!("Library/Logs/Pi/server-{}.log",ui.model.config.port));
            let _=NSWorkspace::sharedWorkspace().openURL(&NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy())));
        }
        #[unsafe(method(signInRadius:))]
        fn signin(&self,_:&AnyObject){ self.sign_in(); }
        #[unsafe(method(signOutRadius:))]
        fn signout(&self,_:&AnyObject){let _=self.ivars().borrow().model.radius.sign_out();}
        #[unsafe(method(reloadPage:))]
        fn reload(&self,_:&AnyObject){ if let Some(page)=self.key_page(){page.reload();} }
        #[unsafe(method(openSettings:))]
        fn settings(&self,_:&AnyObject){ if let Some(page)=self.key_page(){page.event(host_bridge::MENU_EVENT,"item","settings");} }
        #[unsafe(method(setZoom:))]
        fn set_zoom(&self,item:&NSMenuItem){ if let Some(page)=self.key_page(){
            // SAFETY: calls WebKit on a retained view on the main thread.
            unsafe { let zoom = if item.tag()==0{1.0}else{(page.web.pageZoom()+item.tag() as f64*0.1).clamp(0.5,3.0)};page.web.setPageZoom(zoom); }
            page.web.inset();
        }}
        #[unsafe(method(selectBrain:))]
        fn select_brain(&self,item:&NSMenuItem){ if let Some(win)=self.key_window_id() && let Ok(index)=usize::try_from(item.tag()){self.select_tab(win,index,false);} }
        #[unsafe(method(checkUpdates:))]
        fn check_updates(&self,_:&AnyObject){self.update(false);}
    }
);

impl AppDelegate {
    pub fn new(mtm: MainThreadMarker) -> Result<Retained<Self>> {
        let env = environment();
        let bundle = NSBundle::mainBundle();
        let bundled = bundle
            .resourceURL()
            .and_then(|u| u.path())
            .and_then(|s| Bundled::find(Path::new(&s.to_string())));
        let launch = Launch::load(&env, &MacPreferences, bundled.as_ref())?;
        let build = bundle_string("CFBundleVersion").unwrap_or_else(|| "0".into());
        let diagnostics = Diagnostics::new(&launch);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let (send, events) = mpsc::channel();
        let mut model = make_model(launch.selected.clone(), &launch, &env)?;
        let mut directory = BrainDirectory::new(launch.brains.as_deref());
        directory.rebuild(&model.config, false);
        let phase = State {
            phase: Phase::Checking,
            ownership: Ownership::None,
        };
        start_workers(&runtime, &mut model, &launch, &build, 1, &send);
        let alloc = Self::alloc(mtm).set_ivars(RefCell::new(StateUi {
            runtime,
            events,
            send,
            generation: 1,
            env,
            launch,
            bundled,
            build,
            appearance: Appearance::load(&MacPreferences),
            model,
            phase,
            brains: directory.brains,
            shows_tabs: directory.shows_tabs,
            windows: Vec::new(),
            found: Vec::new(),
            next_id: 1,
            status: None,
            timer: None,
            diagnostics,
            changing: false,
            quitting: false,
        }));
        // SAFETY: instance initialized once as NSObject; all ivars already present.
        Ok(unsafe { msg_send![super(alloc), init] })
    }
    fn setup(&self) {
        let app = NSApplication::sharedApplication(self.mtm());
        self.apply_appearance(self.appearance_value());
        main_menu::build(&app, self, self.mtm());
        self.refresh_appearance_menu();
        let status =
            NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        if let Some(button) = status.button(self.mtm()) {
            button.setImage(Some(&glyph()));
            button.setAppearsDisabled(true);
        }
        let menu = NSMenu::new(self.mtm());
        menu.setDelegate(Some(ProtocolObject::from_ref(self)));
        status.setMenu(Some(&menu));
        let weak = Weak::new(self);
        let tick = RcBlock::new(move |_: NonNull<NSTimer>| {
            if let Some(app) = weak.load() {
                app.tick();
            }
        });
        // SAFETY: scheduled on AppKit's main run loop; timer retains the block with its weak app reference.
        let timer =
            unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.1, true, &tick) };
        {
            let mut ui = self.ivars().borrow_mut();
            ui.status = Some(status);
            ui.timer = Some(timer);
            let out = ui.send.clone();
            ui.runtime.spawn(async move {
                if let Ok(mut signal) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                {
                    tokio::select! {_=tokio::signal::ctrl_c()=>{},_=signal.recv()=>{}};
                    let _ = out.send(Event::Quit);
                }
            });
        }
        self.scan();
        self.show();
        if self.ivars().borrow().launch.sign_in_headless {
            self.sign_in();
        }
        self.update(true);
    }
    fn tick(&self) {
        loop {
            let event = self.ivars().borrow().events.try_recv();
            let Ok(event) = event else {
                break;
            };
            match event {
                Event::Phase(g, phase) if self.ivars().borrow().generation == g => {
                    let pages = {
                        let mut ui = self.ivars().borrow_mut();
                        ui.phase = phase;
                        if let Some(button) = ui.status.as_ref().and_then(|s| s.button(self.mtm()))
                        {
                            button.setAppearsDisabled(ui.phase.phase != Phase::Running);
                        }
                        ui.windows
                            .iter()
                            .flat_map(|w| w.pages().cloned())
                            .collect::<Vec<_>>()
                    };
                    for page in pages {
                        self.update_page(&page);
                    }
                }
                Event::Directory(g, brains, show) if self.ivars().borrow().generation == g => {
                    self.update_directory(brains, show)
                }
                Event::Servers(found) => self.ivars().borrow_mut().found = found,
                Event::Loaded(id, result) => {
                    if let Some(page) = self.page(id) {
                        if result.as_ref().is_ok_and(|load| load.gateway.is_some()) {
                            self.renew(&page);
                        }
                        page.finish_load(result);
                    }
                }
                Event::Renewed(id, cookie) => {
                    if let Some(page) = self.page(id) {
                        page.put_cookie(cookie, None);
                    }
                }
                Event::Prompt(message) => self.alert("Pi", &message),
                Event::OpenUrl(url) => {
                    if let Err(error) = open_url(&url) {
                        self.alert("Could not open browser", &error.to_string());
                    }
                }
                Event::Install(path) => {
                    let _ = NSWorkspace::sharedWorkspace().openURL(&NSURL::fileURLWithPath(
                        &NSString::from_str(&path.to_string_lossy()),
                    ));
                }
                Event::Switch(config) => self.apply_switch(config),
                Event::Quit => NSApplication::sharedApplication(self.mtm()).terminate(None),
                Event::QuitReady => {
                    NSApplication::sharedApplication(self.mtm())
                        .replyToApplicationShouldTerminate(true);
                    return;
                }
                Event::Diagnose(id, index) => {
                    if let Some(page) = self.page(id) {
                        self.capture(&page, index);
                    }
                }
                Event::OpenBrain(id, path) => self.open_brain(&id, &path),
                _ => {}
            }
        }
    }
    fn update_directory(&self, brains: Vec<Brain>, show: bool) {
        let fallback = brains.iter().find(|b| b.is_direct()).map(|b| b.id.clone());
        let mut reselect = Vec::new();
        {
            let mut ui = self.ivars().borrow_mut();
            if ui.brains == brains && ui.shows_tabs == show {
                return;
            }
            for window in &mut ui.windows {
                if let Some(page) = window.selected() {
                    if let Some(brain) = brains.iter().find(|b| b.id == page.brain.id) {
                        if brain.route != page.brain.route {
                            reselect.push((window.id, brain.id.clone(), page.current_path()));
                        }
                    } else if let Some(id) = &fallback {
                        reselect.push((window.id, id.clone(), "/".into()));
                    }
                }
                window.refresh(&brains, show);
            }
            ui.brains = brains;
            ui.shows_tabs = show;
        }
        for (win, id, path) in reselect {
            self.select_in(win, &id, &path);
        }
        self.update_shortcuts();
    }
    fn update_shortcuts(&self) {
        let Some(menu) = NSApplication::sharedApplication(self.mtm()).windowsMenu() else {
            return;
        };
        let remove = menu
            .itemArray()
            .iter()
            .filter(|item| item.action() == Some(sel!(selectBrain:)))
            .collect::<Vec<_>>();
        for item in remove {
            menu.removeItem(&item);
        }
        let ui = self.ivars().borrow();
        if !ui.shows_tabs {
            return;
        }
        for (i, brain) in ui.brains.iter().take(9).enumerate().rev() {
            let item = main_menu::item(
                &brain.label(),
                Some(sel!(selectBrain:)),
                &(i + 1).to_string(),
                Some(self),
                self.mtm(),
            );
            item.setTag(i as isize);
            menu.insertItem_atIndex(&item, 0);
        }
    }
    fn scan(&self) {
        let ui = self.ivars().borrow();
        let home = ui.launch.home.join(".pi/p2");
        let out = ui.send.clone();
        ui.runtime.spawn(async move {
            if let Ok(found) = server_scan::scan(&home).await {
                let _ = out.send(Event::Servers(found));
            }
        });
    }
    fn switch(&self, config: ServerConfig) {
        let mut ui = self.ivars().borrow_mut();
        if ui.changing || ui.quitting {
            return;
        }
        if config == ui.model.config {
            return;
        }
        ui.changing = true;
        let out = ui.send.clone();
        let server = ui.model.server.clone();
        ui.runtime.spawn(async move {
            let _ = server.lock().await.stop().await;
            let _ = out.send(Event::Switch(config));
        });
    }
    fn apply_switch(&self, config: ServerConfig) {
        let windows = {
            let mut ui = self.ivars().borrow_mut();
            ui.changing = false;
            let Ok(mut model) = make_model(config.clone(), &ui.launch, &ui.env) else {
                return;
            };
            ui.generation += 1;
            start_workers(
                &ui.runtime,
                &mut model,
                &ui.launch,
                &ui.build,
                ui.generation,
                &ui.send,
            );
            let mut directory = BrainDirectory::new(ui.launch.brains.as_deref());
            directory.rebuild(&config, false);
            ui.brains = directory.brains;
            ui.shows_tabs = directory.shows_tabs;
            ui.model = model;
            ui.phase = State {
                phase: Phase::Checking,
                ownership: Ownership::None,
            };
            let _ = config.save_selection(&mut MacPreferences);
            std::mem::take(&mut ui.windows)
        };
        for window in windows {
            window.native.close();
        }
        self.show();
        self.update_shortcuts();
    }
    fn refresh_own(&self) {
        let config = {
            let mut ui = self.ivars().borrow_mut();
            let Ok(launch) = Launch::load(&ui.env, &MacPreferences, ui.bundled.as_ref()) else {
                return;
            };
            ui.launch.own = launch.own.clone();
            launch.own
        };
        self.switch(config);
    }
    fn fill_status(&self, menu: &NSMenu) {
        let ui = self.ivars().borrow();
        let m = self.mtm();
        menu.removeAllItems();
        let entry = |title: &str, action: Option<Sel>| {
            main_menu::item(title, action, "", action.map(|_| self as &AnyObject), m)
        };
        let label = status_text(&ui.phase.phase);
        menu.addItem(&entry(&label, None));
        menu.addItem(&entry(
            &format!(
                "127.0.0.1:{} · {}",
                ui.model.config.port,
                ui.model.config.data_dir.display()
            ),
            None,
        ));
        menu.addItem(&NSMenuItem::separatorItem(m));
        menu.addItem(&entry("Open Pi", Some(sel!(openPi:))));
        if ui.phase.ownership == Ownership::Managed && ui.phase.phase != Phase::Stopping {
            menu.addItem(&entry("Stop Pi", Some(sel!(stopServer:))));
        }
        if ui.phase.phase == Phase::Running
            && (ui.phase.ownership == Ownership::Managed
                || dev_control::available(&ui.model.config.data_dir))
        {
            menu.addItem(&entry("Restart Pi", Some(sel!(restartServer:))));
        }
        if can_start(&ui.model.config, &ui.phase) {
            menu.addItem(&entry("Start Pi", Some(sel!(startServer:))));
        }
        if ui.model.config.source == Source::Own
            && ui
                .launch
                .home
                .join(format!(
                    "Library/Logs/Pi/server-{}.log",
                    ui.model.config.port
                ))
                .exists()
        {
            menu.addItem(&entry("Show Server Log", Some(sel!(showLog:))));
        }
        if bundle_string("SUFeedURL").is_some() {
            menu.addItem(&entry("Check for Updates…", Some(sel!(checkUpdates:))));
        }
        if ui.launch.brains.is_some() {
            menu.addItem(&entry("Radius: brains from P2MAC_BRAINS", None));
        } else if ui.model.radius.signed_in() {
            menu.addItem(&entry("Sign Out of Radius", Some(sel!(signOutRadius:))));
        } else {
            menu.addItem(&entry("Sign In to Radius…", Some(sel!(signInRadius:))));
        }
        let servers = main_menu::submenu(menu, "Server", m);
        servers.addItem(&entry(
            if ui.found.is_empty() {
                "No running Pi servers found"
            } else {
                "Running on this Mac"
            },
            None,
        ));
        for (i, server) in ui.found.iter().enumerate() {
            let mut title = format!("127.0.0.1:{} · {}", server.port, server.data_dir.display());
            if let Some(checkout) = &server.checkout {
                title.push_str(&format!(" · {}", checkout.display()));
            }
            if server.dev {
                title.push_str(" · dev");
            }
            let item = entry(&title, Some(sel!(useFound:)));
            item.setTag(i as isize);
            item.setState(
                if ui.model.config.source == Source::Attached && ui.model.config.port == server.port
                {
                    NSControlStateValueOn
                } else {
                    NSControlStateValueOff
                },
            );
            servers.addItem(&item);
        }
        servers.addItem(&NSMenuItem::separatorItem(m));
        let label = if ui.launch.own.runs_bundled {
            "Run Pi from This App".into()
        } else {
            ui.launch.own.checkout.as_ref().map_or_else(
                || "Run Pi from a Checkout".into(),
                |p| format!("Run Pi from {}", p.display()),
            )
        };
        servers.addItem(&entry(
            &label,
            ui.launch.own.checkout.as_ref().map(|_| sel!(useOwn:)),
        ));
        servers.addItem(&entry("Choose p2 Checkout…", Some(sel!(chooseCheckout:))));
        if ui.bundled.is_some() && !ui.launch.own.runs_bundled {
            servers.addItem(&entry(
                "Use This App's Pi Instead of the Checkout",
                Some(sel!(forgetCheckout:)),
            ));
        }
        menu.addItem(&NSMenuItem::separatorItem(m));
        menu.addItem(&main_menu::item(
            "Quit Pi",
            Some(sel!(terminate:)),
            "q",
            Some(&NSApplication::sharedApplication(m)),
            m,
        ));
    }
    fn show(&self) {
        let (window, local) = {
            let ui = self.ivars().borrow();
            (
                ui.windows.last().map(|w| w.native.clone()),
                ui.brains
                    .iter()
                    .find(|b| b.is_direct())
                    .map(|b| b.id.clone()),
            )
        };
        if let Some(window) = window {
            window.makeKeyAndOrderFront(None);
            NSApplication::sharedApplication(self.mtm()).activate();
        } else if let Some(id) = local {
            self.open_brain(&id, "/");
        }
    }
    pub(super) fn open_brain(&self, id: &str, path: &str) {
        let mtm = self.mtm();
        let (wid, native) = {
            let mut ui = self.ivars().borrow_mut();
            if ui.quitting {
                return;
            }
            let wid = ui.next_id;
            ui.next_id += 1;
            let win = Window::new(wid, self, mtm);
            if let Some(prev) = ui.windows.last() {
                let frame = prev.native.frame();
                win.native.setFrame_display(frame, false);
                win.native.cascadeTopLeftFromPoint(NSPoint::new(
                    frame.origin.x,
                    frame.origin.y + frame.size.height,
                ));
            } else {
                let name = NSString::from_str("Pi.main");
                win.native.setFrameAutosaveName(&name);
                if !win.native.setFrameUsingName(&name) {
                    win.native.center();
                }
            }
            let native = win.native.clone();
            ui.windows.push(win);
            (wid, native)
        };
        self.select_in(wid, id, path);
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        native.makeKeyAndOrderFront(None);
        app.activate();
    }
    pub(super) fn close_window(&self, id: u64) {
        let Ok(mut ui) = self.ivars().try_borrow_mut() else {
            return;
        };
        ui.windows.retain(|w| w.id != id);
        if ui.windows.is_empty() {
            NSApplication::sharedApplication(self.mtm())
                .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        }
    }
    pub(super) fn layout_window(&self, id: u64) {
        if let Ok(ui) = self.ivars().try_borrow()
            && let Some(w) = ui.windows.iter().find(|w| w.id == id)
        {
            w.layout();
        }
    }
    pub(super) fn select_tab(&self, window: u64, index: usize, detach: bool) {
        let id = {
            let ui = self.ivars().borrow();
            ui.windows
                .iter()
                .find(|w| w.id == window)
                .and_then(|w| w.clicked(index))
                .map(str::to_owned)
        };
        if let Some(id) = id {
            if detach {
                self.open_brain(&id, "/");
            } else {
                self.select_in(window, &id, "/");
            }
        }
    }
    fn select_in(&self, id: u64, brain_id: &str, path: &str) {
        let result = {
            let mut ui = self.ivars().borrow_mut();
            let page_id = ui.next_id;
            ui.next_id += 1;
            let StateUi {
                brains,
                shows_tabs,
                windows,
                ..
            } = &mut *ui;
            let Some(brain) = brains.iter().find(|b| b.id == brain_id).cloned() else {
                return;
            };
            let Some(window) = windows.iter_mut().find(|w| w.id == id) else {
                return;
            };
            window.select(brain, path, self, page_id, brains, *shows_tabs)
        };
        match result {
            Ok(page) => self.update_page(&page),
            Err(error) => self.alert("Could not open Pi", &error.to_string()),
        }
    }
    fn page(&self, id: u64) -> Option<Rc<Page>> {
        self.ivars()
            .borrow()
            .windows
            .iter()
            .find_map(|w| w.page(id))
    }
    fn key_window_id(&self) -> Option<u64> {
        let key = NSApplication::sharedApplication(self.mtm()).keyWindow()?;
        self.ivars()
            .borrow()
            .windows
            .iter()
            .find(|w| std::ptr::eq(&*w.native, &*key))
            .map(|w| w.id)
    }
    fn key_page(&self) -> Option<Rc<Page>> {
        let id = self.key_window_id()?;
        self.ivars()
            .borrow()
            .windows
            .iter()
            .find(|w| w.id == id)?
            .selected()
    }
    fn update_page(&self, page: &Page) {
        if page.is_loaded() {
            return;
        }
        if !page.brain.is_direct() {
            page.start();
            return;
        }
        let (running, label, button) = {
            let ui = self.ivars().borrow();
            (
                ui.phase.phase == Phase::Running,
                status_text(&ui.phase.phase),
                can_start(&ui.model.config, &ui.phase),
            )
        };
        if running {
            page.start();
        } else {
            page.status(
                &if button {
                    format!("{label} · Start Pi")
                } else {
                    label
                },
                button,
            );
        }
    }
    pub(super) fn load_page(&self, id: u64, brain: Brain, path: String) {
        let ui = self.ivars().borrow();
        let account = ui.model.radius.clone();
        let out = ui.send.clone();
        ui.runtime.spawn(async move {
            let result = match brain.route {
                Route::Direct { origin, data_dir } => {
                    match local_sign_in::cookie(&origin, &data_dir).await {
                        Ok(cookie) => origin
                            .join(&path)
                            .map(|url| Load {
                                url,
                                cookie: Some(cookie),
                                gateway: None,
                            })
                            .map_err(Error::from),
                        Err(e) => Err(e),
                    }
                }
                Route::Remote {
                    organization,
                    server,
                } => account
                    .open_url(&organization, &server, &path)
                    .await
                    .map(|url| Load {
                        url,
                        cookie: None,
                        gateway: Some(account.gateway()),
                    }),
            };
            let _ = out.send(Event::Loaded(id, result));
        });
    }
    fn renew(&self, page: &Page) {
        let Route::Remote {
            organization,
            server,
        } = &page.brain.route
        else {
            return;
        };
        let (org, server, id) = (organization.clone(), server.clone(), page.id);
        if let Some(old) = page.renewal.borrow_mut().take() {
            old.abort();
        }
        let ui = self.ivars().borrow();
        let radius = ui.model.radius.clone();
        let out = ui.send.clone();
        *page.renewal.borrow_mut() = Some(ui.runtime.spawn(async move {
            let mut wait = view_cookie::RENEW_EVERY;
            loop {
                tokio::time::sleep(wait).await;
                let result = match radius.open_url(&org, &server, "/").await {
                    Ok(url) => view_cookie::exchange(&url).await,
                    Err(e) => Err(e),
                };
                wait = match result {
                    Ok(cookie) => {
                        if out.send(Event::Renewed(id, cookie)).is_err() {
                            break;
                        }
                        view_cookie::RENEW_EVERY
                    }
                    Err(e) => {
                        eprintln!("Pi: view cookie renewal failed: {e}");
                        view_cookie::RETRY_AFTER
                    }
                };
            }
        }));
    }
    pub(super) fn server_running(&self) -> bool {
        self.ivars().borrow().phase.phase == Phase::Running
    }
    pub(super) fn start_server(&self) {
        let ui = self.ivars().borrow();
        if ui.changing || ui.quitting {
            return;
        }
        let server = ui.model.server.clone();
        let build = ui.build.clone();
        let out = ui.send.clone();
        ui.runtime.spawn(async move {
            let mut server = server.lock().await;
            if server.can_start() {
                server.prepare_upgrade(&build, &mut MacPreferences);
                if let Err(e) = server.start().await {
                    let _ = out.send(Event::Prompt(e.to_string()));
                }
            }
        });
    }
    fn server_action(&self, restart: bool) {
        let ui = self.ivars().borrow();
        let server = ui.model.server.clone();
        let out = ui.send.clone();
        ui.runtime.spawn(async move {
            let mut server = server.lock().await;
            let result = if restart {
                server.restart().await
            } else {
                server.stop().await
            };
            if let Err(e) = result {
                let _ = out.send(Event::Prompt(e.to_string()));
            }
        });
    }
    fn sign_in(&self) {
        let ui = self.ivars().borrow();
        let account = ui.model.radius.clone();
        let out = ui.send.clone();
        let headless = ui.launch.sign_in_headless;
        ui.runtime.spawn(async move {
            let result = account
                .sign_in(headless, |url| {
                    out.send(Event::OpenUrl(url.clone()))
                        .map_err(|_| Error::Protocol("app is closing".into()))
                })
                .await;
            if let Err(e) = result {
                let _ = out.send(Event::Prompt(format!("Could not sign in to Radius: {e}")));
            }
        });
    }
    pub(super) fn appearance_value(&self) -> Appearance {
        self.ivars().borrow().appearance
    }
    fn apply_appearance(&self, appearance: Appearance) {
        let app = NSApplication::sharedApplication(self.mtm());
        // SAFETY: constants are retained framework appearance names on macOS 14+.
        let style = unsafe {
            match appearance {
                Appearance::System => None,
                Appearance::Light => NSAppearance::appearanceNamed(NSAppearanceNameAqua),
                Appearance::Dark => NSAppearance::appearanceNamed(NSAppearanceNameDarkAqua),
            }
        };
        app.setAppearance(style.as_deref());
    }
    pub(super) fn set_appearance(&self, appearance: Appearance) -> Result<()> {
        appearance.save(&mut MacPreferences)?;
        let reduce = NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion();
        let pages = {
            let mut ui = self.ivars().borrow_mut();
            ui.appearance = appearance;
            let mut pages = Vec::new();
            for window in &ui.windows {
                window.fade(reduce);
                for page in window.pages() {
                    page.fade(reduce);
                    pages.push(page.clone());
                }
            }
            pages
        };
        self.apply_appearance(appearance);
        for page in pages {
            page.event(host_bridge::THEME_EVENT, "preference", appearance.name());
        }
        self.refresh_appearance_menu();
        Ok(())
    }
    fn refresh_appearance_menu(&self) {
        let Some(menu) = NSApplication::sharedApplication(self.mtm())
            .mainMenu()
            .and_then(|menu| menu.itemWithTitle(&NSString::from_str("View")))
            .and_then(|item| item.submenu())
            .and_then(|menu| menu.itemWithTitle(&NSString::from_str("Appearance")))
            .and_then(|item| item.submenu())
        else {
            return;
        };
        for (i, item) in menu.itemArray().iter().enumerate() {
            item.setState(
                if Appearance::ALL.get(i) == Some(&self.appearance_value()) {
                    NSControlStateValueOn
                } else {
                    NSControlStateValueOff
                },
            );
        }
    }
    fn alert(&self, title: &str, message: &str) {
        NSApplication::sharedApplication(self.mtm()).activate();
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(message));
        alert.runModal();
    }
    fn update(&self, daily: bool) {
        let Some(feed) = bundle_string("SUFeedURL").and_then(|s| Url::parse(&s).ok()) else {
            return;
        };
        let Some(key) = bundle_string("SUPublicEDKey") else {
            return;
        };
        let ui = self.ivars().borrow();
        let out = ui.send.clone();
        let build = ui.build.clone();
        let os = NSProcessInfo::processInfo().operatingSystemVersion();
        let system = format!(
            "{}.{}.{}",
            os.majorVersion, os.minorVersion, os.patchVersion
        );
        ui.runtime.spawn(async move {
            loop {
                if daily {
                    tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
                }
                let arch = if cfg!(target_arch = "aarch64") {
                    "arm64"
                } else {
                    "x86_64"
                };
                let found = updater::check(&feed, &build, Some(&system), arch).await;
                match found {
                    Ok(Some(update)) if daily => {
                        let _ = out.send(Event::Prompt(format!(
                            "Pi {} is available. Use Check for Updates to download it.",
                            update.version
                        )));
                    }
                    Ok(Some(update)) => {
                        let result = async {
                            let bytes = updater::download_verified(&update, &key).await?;
                            let mut file = tempfile::Builder::new()
                                .prefix("pi-update-")
                                .suffix(".dmg")
                                .tempfile()?;
                            file.write_all(&bytes)?;
                            file.as_file().sync_all()?;
                            file.keep()
                                .map(|(_, path)| path)
                                .map_err(|e| Error::Io(e.error))
                        }
                        .await;
                        let _ = out.send(match result {
                            Ok(path) => Event::Install(path),
                            Err(e) => Event::Prompt(e.to_string()),
                        });
                    }
                    Ok(None) if !daily => {
                        let _ = out.send(Event::Prompt("Pi is up to date".into()));
                    }
                    Err(e) if !daily => {
                        let _ = out.send(Event::Prompt(e.to_string()));
                    }
                    _ => {}
                }
                if !daily {
                    break;
                }
            }
        });
    }
    pub(super) fn diagnose(&self, page: &Page) {
        let mut ui = self.ivars().borrow_mut();
        let Some(diag) = &mut ui.diagnostics else {
            return;
        };
        let (index, open) = diag.next_page();
        let path = diag.open_path.clone();
        let id = page.id;
        let out = ui.send.clone();
        ui.runtime.spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let _ = out.send(Event::Diagnose(id, index));
            if let Some(open) = open {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let _ = out.send(Event::OpenBrain(open, path));
            }
        });
    }
    fn capture(&self, page: &Page, index: usize) {
        let weak = Weak::new(self);
        let id = page.id;
        diagnostic_js(&page.web, PAGE_PROBE, move |value| {
            let Some(app) = weak.load() else {
                return;
            };
            let Some(page) = app.page(id) else {
                return;
            };
            let Ok(mut capture) = serde_json::from_value::<PageCapture>(value) else {
                return;
            };
            capture.app_appearance = Some(app.diagnostic_appearance());
            capture.chrome = Some(capture_chrome(&page));
            let (directory, bridge) = {
                let mut ui = app.ivars().borrow_mut();
                let Some(diag) = &mut ui.diagnostics else {
                    return;
                };
                let _ = diag.page(index, &capture);
                (
                    diag.directory().to_owned(),
                    diag.consume_bridge(capture.pi_host == Some(true)),
                )
            };
            capture_images(&page, &directory, &index.to_string());
            if let Some(preference) = bridge {
                app.capture_bridge(&page, index, &preference);
            }
        });
    }
    fn capture_bridge(&self, page: &Page, index: usize, preference: &str) {
        let weak = Weak::new(self);
        let id = page.id;
        let received = RcBlock::new(
            move |value: *mut AnyObject, _: *mut objc2_foundation::NSError| {
                // SAFETY: WebKit provides a valid object for the duration of the completion handler, or null.
                let Some(value) = unsafe { value.as_ref() }.and_then(json_value) else {
                    return;
                };
                let Some(app) = weak.load() else {
                    return;
                };
                let Some(page) = app.page(id) else {
                    return;
                };
                let native = app.diagnostic_appearance();
                let weak = Weak::new(&*app);
                probe_foreign(&page.web, move |foreign| {
                    let Some(app) = weak.load() else {
                        return;
                    };
                    let Some(page) = app.page(id) else {
                        return;
                    };
                    let record = BridgeCapture {
                        page: value,
                        app_appearance: Some(native),
                        foreign,
                        app_appearance_after_refusals: Some(app.diagnostic_appearance()),
                        ..BridgeCapture::default()
                    };
                    let directory = {
                        let ui = app.ivars().borrow();
                        let Some(diag) = &ui.diagnostics else {
                            return;
                        };
                        diag.directory().to_owned()
                    };
                    capture_images(&page, &directory, &format!("{index}-bridge"));
                    app.capture_theme_controls(&page, index, record);
                });
            },
        );
        let key = NSString::from_str("preference");
        let value = NSString::from_str(preference);
        let args = NSDictionary::from_slices(&[&*key], &[&*value as &AnyObject]);
        // SAFETY: argument strings are passed as objects, never interpolated into script; callback is copied.
        unsafe {
            page.web
                .callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
                    &NSString::from_str(BRIDGE_PROBE),
                    Some(&args),
                    None,
                    &WKContentWorld::pageWorld(self.mtm()),
                    Some(&received),
                );
        }
    }

    fn diagnostic_appearance(&self) -> String {
        NSApplication::sharedApplication(self.mtm())
            .effectiveAppearance()
            .name()
            .to_string()
    }

    fn capture_theme_controls(&self, page: &Page, index: usize, mut record: BridgeCapture) {
        let app = NSApplication::sharedApplication(self.mtm());
        let menu = app
            .mainMenu()
            .and_then(|m| m.itemWithTitle(&NSString::from_str("View")))
            .and_then(|item| item.submenu())
            .and_then(|m| m.itemWithTitle(&NSString::from_str("Appearance")))
            .and_then(|item| item.submenu());
        let checked = |menu: Option<&NSMenu>| {
            menu.map(|menu| {
                menu.itemArray()
                    .iter()
                    .filter(|item| item.state() == NSControlStateValueOn)
                    .map(|item| item.title().to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
        };
        let before = checked(menu.as_deref());
        let weak = Weak::new(self);
        let id = page.id;
        // Install the page listener before actuating AppKit's menu action.
        diagnostic_js(
            &page.web,
            "window.piDiagEvents=[];window.piDiagEvent=e=>window.piDiagEvents.push(e.detail);addEventListener('pi-host-theme',window.piDiagEvent);return {};",
            move |_| {
                let Some(app) = weak.load() else {
                    return;
                };
                let Some(page) = app.page(id) else {
                    return;
                };
                if let Some(menu) = &menu
                    && menu.numberOfItems() > 0
                {
                    menu.performActionForItemAtIndex(0);
                }
                let checked_after = checked(menu.as_deref());
                let appearance = NSApplication::sharedApplication(app.mtm())
                    .appearance()
                    .map_or_else(
                        || "nil (system)".into(),
                        |appearance| appearance.name().to_string(),
                    );
                diagnostic_js(&page.web, THEME_CONTROL_PROBE, move |value| {
                    record.menu = serde_json::json!({
                        "checked": before, "checkedAfterSystem": checked_after,
                        "appAppearanceAfterSystem": appearance, "eventsAfterSystem": value.get("events"),
                    });
                    record.switches = value.get("switch").cloned().unwrap_or_default();
                    record.fade = value.get("fade").cloned().unwrap_or_default();
                    if let Some(app) = weak.load()
                        && let Some(diag) = &app.ivars().borrow().diagnostics
                    {
                        let _ = diag.bridge(index, &record);
                    }
                });
            },
        );
    }
}

fn bundle_string(key: &str) -> Option<String> {
    NSBundle::mainBundle()
        .objectForInfoDictionaryKey(&NSString::from_str(key))?
        .downcast::<NSString>()
        .ok()
        .map(|s| s.to_string())
}
fn can_start(config: &ServerConfig, state: &State) -> bool {
    config.source == Source::Own
        && config.checkout.is_some()
        && state.ownership == Ownership::None
        && matches!(
            state.phase,
            Phase::Stopped | Phase::Checking | Phase::Failed(_)
        )
}
fn status_text(phase: &Phase) -> String {
    match phase {
        Phase::Checking => "Looking for Pi…".into(),
        Phase::Stopped => "Pi is stopped".into(),
        Phase::Starting => "Pi is starting…".into(),
        Phase::Running => "Pi is running · On this Mac".into(),
        Phase::Stopping => "Pi is stopping…".into(),
        Phase::Failed(message) => format!("Pi is not running: {message}"),
    }
}

fn make_model(config: ServerConfig, launch: &Launch, env: &Environment) -> Result<Models> {
    let server = ServerController::new(config.clone(), &launch.home, env.clone())?;
    let radius = RadiusAccount::new(&config.data_dir, launch.gateway.clone())?;
    Ok(Models {
        config,
        radius,
        server: Arc::new(Mutex::new(server)),
        tasks: Vec::new(),
    })
}
fn start_workers(
    runtime: &Runtime,
    model: &mut Models,
    launch: &Launch,
    build: &str,
    generation: u64,
    events: &mpsc::Sender<Event>,
) {
    let server = model.server.clone();
    let out = events.clone();
    let build = build.to_owned();
    let (watch, ready) = oneshot::channel();
    model.tasks.push(runtime.spawn(async move {
        let mut server = server.lock().await;
        let _ = watch.send(server.changes());
        let _ = server.poll().await;
        if server.can_start() {
            server.prepare_upgrade(&build, &mut MacPreferences);
            let _ = server.start().await;
        }
        drop(server);
    }));
    model.tasks.push(runtime.spawn(async move {
        if let Ok(mut changes) = ready.await {
            loop {
                let state = changes.borrow_and_update().clone();
                if out.send(Event::Phase(generation, state)).is_err()
                    || changes.changed().await.is_err()
                {
                    break;
                }
            }
        }
    }));
    let server = model.server.clone();
    model.tasks.push(runtime.spawn(async move {
        let mut timer = tokio::time::interval(PROBE_INTERVAL);
        loop {
            timer.tick().await;
            let _ = server.lock().await.poll().await;
        }
    }));
    let server = model.server.clone();
    let account = model.radius.clone();
    let config = model.config.clone();
    let out = events.clone();
    let fixture = launch.brains.clone();
    model.tasks.push(runtime.spawn(async move {
        let mut directory = BrainDirectory::new(fixture.as_deref());
        let mut timer = tokio::time::interval(POLL_INTERVAL);
        loop {
            timer.tick().await;
            let running = server.lock().await.state().phase == Phase::Running;
            let _ = directory.poll(&account, &config, running).await;
            if out
                .send(Event::Directory(
                    generation,
                    directory.brains.clone(),
                    directory.shows_tabs,
                ))
                .is_err()
            {
                break;
            }
        }
    }));
}

fn glyph() -> Retained<NSImage> {
    let draw = RcBlock::new(|r: NSRect| {
        let cell = r.size.width / 4.0;
        NSColor::blackColor().setFill();
        for [x, y, w, h] in pi_glyph::CELLS {
            NSBezierPath::fillRect(NSRect::new(
                NSPoint::new(f64::from(x) * cell, f64::from(y) * cell),
                NSSize::new(f64::from(w) * cell, f64::from(h) * cell),
            ));
        }
        objc2::runtime::Bool::YES
    });
    let icon = NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(15.0, 15.0), true, &draw);
    icon.setTemplate(true);
    icon
}

pub(super) fn fade_view(view: &NSView, reduce: bool) {
    let Some(duration) = appearance::fade(true, !view.isHidden(), reduce) else {
        return;
    };
    view.setWantsLayer(true);
    if let Some(layer) = view.layer() {
        let animation = objc2_quartz_core::CATransition::new();
        // SAFETY: kCATransitionFade is a static Core Animation NSString constant.
        animation.setType(unsafe { objc2_quartz_core::kCATransitionFade });
        animation.setDuration(duration.as_secs_f64());
        layer.addAnimation_forKey(&animation, Some(&NSString::from_str("pi-appearance")));
    }
}

fn diagnostic_js(web: &WKWebView, script: &str, finish: impl FnOnce(serde_json::Value) + 'static) {
    let finish = std::cell::Cell::new(Some(finish));
    let receive = RcBlock::new(
        move |value: *mut AnyObject, error: *mut objc2_foundation::NSError| {
            let Some(finish) = finish.take() else {
                return;
            };
            // SAFETY: non-null completion pointers remain live for the callback scope.
            let value = unsafe { value.as_ref() }.and_then(json_value).unwrap_or_else(|| {
            // SAFETY: the error pointer has the same callback scope.
            serde_json::json!({"error":unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string())})
        });
            finish(value);
        },
    );
    // SAFETY: the live view is on the main thread and WebKit copies the completion block.
    unsafe {
        web.callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
            &NSString::from_str(script),
            None,
            None,
            &WKContentWorld::pageWorld(web.mtm()),
            Some(&receive),
        );
    }
}

fn probe_foreign(web: &WKWebView, finish: impl FnOnce(serde_json::Value) + 'static) {
    let mtm = web.mtm();
    // SAFETY: initialize the diagnostic view on the main thread with the real page's configuration.
    // The foreign main frame therefore tests the same message handler; no request goes to the network.
    let foreign = unsafe {
        let configuration = web.configuration();
        let view = WKWebView::initWithFrame_configuration(
            WKWebView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(200.0, 200.0)),
            &configuration,
        );
        let url = NSURL::URLWithString(&NSString::from_str("https://foreign.invalid/"));
        view.loadHTMLString_baseURL(&NSString::from_str("<p>foreign</p>"), url.as_deref());
        view
    };
    let until = Instant::now() + Duration::from_secs(5);
    let finish = std::cell::Cell::new(Some(finish));
    let poll = RcBlock::new(move |timer: NonNull<NSTimer>| {
        // SAFETY: both the timer and retained view belong to the main run loop.
        let ready = unsafe { !foreign.isLoading() && foreign.URL().is_some() };
        if !ready && Instant::now() < until {
            return;
        }
        // SAFETY: the timer is borrowed from Foundation and remains live throughout this callback.
        unsafe { timer.as_ref() }.invalidate();
        let Some(finish) = finish.take() else {
            return;
        };
        if !ready {
            finish(serde_json::json!({"error":"foreign diagnostic load timed out"}));
            return;
        }
        let retained = foreign.clone();
        diagnostic_js(&foreign, FOREIGN_PROBE, move |value| {
            finish(value);
            drop(retained);
        });
    });
    // SAFETY: the run loop owns the copied block, which retains the view until loading ends or times out.
    unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.1, true, &poll);
    }
}

fn capture_chrome(page: &Page) -> serde_json::Value {
    let Some(window) = page.web.window() else {
        return serde_json::json!({});
    };
    // SAFETY: the window's content view is retained while retrieving its AppKit frame view.
    let Some(frame) = window
        .contentView()
        .and_then(|view| unsafe { view.superview() })
    else {
        return serde_json::json!({});
    };
    let size = window.frame().size;
    let hits = [
        ("left", 40.0),
        ("middle", size.width / 2.0),
        ("right", size.width - 30.0),
    ]
    .into_iter()
    .map(|(name, x)| {
        let class = frame
            .hitTest(NSPoint::new(x, size.height - 26.0))
            .map_or_else(
                || "nil".into(),
                |view| view.class().name().to_string_lossy().into_owned(),
            );
        (name, class)
    })
    .collect::<std::collections::BTreeMap<_, _>>();
    serde_json::json!({
        "titlebar": size.height - window.contentLayoutRect().size.height,
        "hits": hits,
        "interactive": page.web.rects()
    })
}

fn capture_images(page: &Page, dir: &Path, name: &str) {
    if let Some(view) = page.web.window().and_then(|window| window.contentView())
        && let Some(bitmap) = view.bitmapImageRepForCachingDisplayInRect(view.bounds())
    {
        view.cacheDisplayInRect_toBitmapImageRep(view.bounds(), &bitmap);
        png(&bitmap, &dir.join(format!("window-{name}.png")));
    }
    let file = dir.join(format!("page-{name}.png"));
    let image = RcBlock::new(
        move |image: *mut NSImage, _: *mut objc2_foundation::NSError| {
            // SAFETY: non-null images are live for this completion callback.
            if let Some(image) = unsafe { image.as_ref() }
                && let Some(data) = image.TIFFRepresentation()
                && let Some(bitmap) =
                    NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &data)
            {
                png(&bitmap, &file);
            }
        },
    );
    // SAFETY: snapshot callback is copied by WebKit; it captures an owned path.
    unsafe {
        page.web
            .takeSnapshotWithConfiguration_completionHandler(None, &image);
    }
}
fn png(bitmap: &NSBitmapImageRep, file: &Path) {
    // SAFETY: empty image-property dictionary is valid for PNG encoding.
    if let Some(data) = unsafe {
        bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    } {
        let _ = std::fs::write(file, data.to_vec());
    }
}

const PAGE_PROBE: &str = r#"const root=document.documentElement;
const record={
    url:location.href,title:document.title,text:document.body?.innerText.slice(0,600)??null,
    notification:typeof Notification==='function'?Notification.permission:'none',
    serviceWorker:'serviceWorker'in navigator,clipboard:!!navigator.clipboard?.writeText,
    getUserMedia:!!navigator.mediaDevices?.getUserMedia,userAgent:navigator.userAgent,
    shell:root.dataset.shell??null,inset:getComputedStyle(root).getPropertyValue('--shell-inset-start'),
    sidebarPadding:(()=>{const e=document.querySelector('.sidebar-root .drawer-top');return e?getComputedStyle(e).paddingLeft:null})(),
    theme:root.dataset.theme??null,colorScheme:root.dataset.colorScheme??null,
    prefersDark:matchMedia('(prefers-color-scheme: dark)').matches,piHost:!!window.webkit?.messageHandlers?.piHost
};
if(document.body){
    const marked=()=>[...document.querySelectorAll('[data-shell-corner]')].map(e=>`${e.className} ${getComputedStyle(e).paddingLeft}`);
    const stack=document.elementsFromPoint(2,26).map(e=>{const r=e.getBoundingClientRect();return `${e.tagName}.${e.className} ${r.left},${r.top} ${r.width}x${r.height}`});
    const wait=()=>new Promise(r=>setTimeout(r,300)),before=marked();
    const layer=document.createElement('div');layer.className='viewer-layer';
    layer.innerHTML='<div class="viewer-panel"><div class="viewer-head"><div><strong>Probe</strong></div></div></div>';
    document.body.append(layer);
    let withViewer;
    try{await wait();withViewer=marked()}finally{layer.remove()}
    await wait();record.corner={stack,before,withViewer,after:marked()};
}
return record;"#;
const BRIDGE_PROBE: &str = r#"const host=window.webkit.messageHandlers.piHost;
const call=async (body,target=host)=>{try{return {reply:await target.postMessage(body)}}catch(e){return {error:String(e)}}};
const capture=()=>({theme:document.documentElement.dataset.theme??null,prefersDark:matchMedia('(prefers-color-scheme: dark)').matches});
const events=[];const onEvent=e=>events.push(e.detail);addEventListener('pi-host-theme',onEvent);
const before={...capture(),get:await call({type:'theme'})};const set=await call({type:'set-theme',preference});
await new Promise(r=>setTimeout(r,1000));const after={...capture(),get:await call({type:'theme'})};
const invalid=await call({type:'set-theme',preference:'sepia'}),unknown=await call({type:'launch'});
const frame=document.createElement('iframe');frame.srcdoc='<p>Pi probe</p>';await new Promise(r=>{frame.onload=r;document.body.append(frame)});
const fromFrame=await call({type:'set-theme',preference:'light'},frame.contentWindow.webkit.messageHandlers.piHost);
frame.remove();removeEventListener('pi-host-theme',onEvent);return {before,set,after,events,invalid,unknown,fromFrame};"#;

const FOREIGN_PROBE: &str = r#"const host=window.webkit?.messageHandlers?.piHost;
if(!host)return {piHost:false};
try{return {piHost:true,origin:location.origin,reply:await host.postMessage({type:'set-theme',preference:'light'})}}
catch(e){return {piHost:true,origin:location.origin,error:String(e)}};"#;

const THEME_CONTROL_PROBE: &str = r#"const host=window.webkit.messageHandlers.piHost;
const wait=ms=>new Promise(r=>setTimeout(r,ms));
const capture=async()=>({theme:document.documentElement.dataset.theme,prefersDark:matchMedia('(prefers-color-scheme: dark)').matches,appPreference:(await host.postMessage({type:'theme'})).preference});
await wait(500);
const events=window.piDiagEvents;
removeEventListener('pi-host-theme',window.piDiagEvent);delete window.piDiagEvent;delete window.piDiagEvents;
const button=document.querySelector('.theme-toggle'),switches=[];
for(const clicks of [1,1,2]){
    if(!button){switches.push({button:false});continue}
    const before=button.title;
    for(let i=0;i<clicks;i++){if(i)await wait(50);button.click()}
    await wait(1500);switches.push({clicks,before,after:button.title,...await capture()});
}
const clock=()=>performance.timeOrigin+performance.now();
const query=matchMedia('(prefers-color-scheme: dark)');
const fade={support:{viewTransition:typeof document.startViewTransition==='function',reducedMotion:matchMedia('(prefers-reduced-motion: reduce)').matches}};
for(const [name,pref] of [['button-to-dark',null],['bridge-to-light','light'],['bridge-to-dark','dark'],['button-to-system',null]]){
    if(!pref&&!button)continue;
    const page={events:[],themeWrites:[],mediaChanges:[]};
    const theme=e=>page.events.push({t:clock(),preference:e.detail?.preference});
    const media=e=>page.mediaChanges.push({t:clock(),dark:e.matches});
    const observer=new MutationObserver(()=>page.themeWrites.push({t:clock(),theme:document.documentElement.dataset.theme}));
    addEventListener('pi-host-theme',theme);query.addEventListener('change',media);
    observer.observe(document.documentElement,{attributes:true,attributeFilter:['data-theme']});
    const start=clock();
    try{
        if(pref)await host.postMessage({type:'set-theme',preference:pref});else button.click();
        await wait(320);fade[name]={start,page,...await capture()};
    }finally{
        observer.disconnect();removeEventListener('pi-host-theme',theme);query.removeEventListener('change',media);
    }
}
return {events,switch:switches,fade};"#;

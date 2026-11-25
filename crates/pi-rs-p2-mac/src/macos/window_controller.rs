use super::{
    app_delegate::AppDelegate,
    brain_page::Page,
    brain_tab_bar::{BrainTabBar, HEIGHT},
};
use crate::{Result, brain::Brain};
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Retained, Weak},
    runtime::{NSObjectProtocol, ProtocolObject},
};
use objc2_app_kit::{
    NSBackingStoreType, NSToolbar, NSView, NSWindow, NSWindowButton, NSWindowDelegate,
    NSWindowStyleMask, NSWindowTabbingMode, NSWindowTitleVisibility, NSWindowToolbarStyle,
};
use objc2_foundation::{NSNotification, NSObject, NSPoint, NSRect, NSSize, NSString};
use std::{collections::VecDeque, rc::Rc};

pub(super) struct Window {
    pub id: u64,
    pub native: Retained<NSWindow>,
    root: Retained<NSView>,
    _hooks: Retained<WindowHooks>,
    tabs: BrainTabBar,
    pages: VecDeque<Rc<Page>>,
    show_tabs: bool,
}
impl Window {
    pub fn new(id: u64, app: &AppDelegate, mtm: MainThreadMarker) -> Self {
        // SAFETY: valid AppKit lifetime and a stored window; disabling release-on-close leaves release to Retained.
        let native = unsafe {
            let win = NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::ZERO, NSSize::new(1280.0, 860.0)),
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable
                    | NSWindowStyleMask::Resizable
                    | NSWindowStyleMask::FullSizeContentView,
                NSBackingStoreType::Buffered,
                false,
            );
            win.setReleasedWhenClosed(false);
            win
        };
        native.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        native.setTitlebarAppearsTransparent(true);
        native.setTabbingMode(NSWindowTabbingMode::Disallowed);
        native.setMinSize(NSSize::new(420.0, 360.0));
        let toolbar =
            NSToolbar::initWithIdentifier(NSToolbar::alloc(mtm), &NSString::from_str("pi-window"));
        native.setToolbar(Some(&toolbar));
        native.setToolbarStyle(NSWindowToolbarStyle::Unified);
        let root = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
        native.setContentView(Some(&root));
        let alloc = WindowHooks::alloc(mtm).set_ivars((Weak::new(app), id));
        // SAFETY: initialize the NSObject subclass before installing it as the weak AppKit delegate.
        let hooks: Retained<WindowHooks> = unsafe { msg_send![super(alloc), init] };
        native.setDelegate(Some(ProtocolObject::from_ref(&*hooks)));
        let tabs = BrainTabBar::new(&hooks, mtm);
        root.addSubview(&tabs.view);
        Self {
            id,
            native,
            root,
            _hooks: hooks,
            tabs,
            pages: VecDeque::new(),
            show_tabs: false,
        }
    }
    pub fn selected(&self) -> Option<Rc<Page>> {
        self.pages.back().cloned()
    }
    pub fn pages(&self) -> impl Iterator<Item = &Rc<Page>> {
        self.pages.iter()
    }
    pub fn page(&self, id: u64) -> Option<Rc<Page>> {
        self.pages.iter().find(|p| p.id == id).cloned()
    }
    pub fn clicked(&self, tag: usize) -> Option<&str> {
        self.tabs.brain(tag)
    }
    pub fn select(
        &mut self,
        brain: Brain,
        path: &str,
        app: &AppDelegate,
        id: u64,
        directory: &[Brain],
        show_tabs: bool,
    ) -> Result<Rc<Page>> {
        let selected = if let Some(index) = self.pages.iter().position(|p| p.brain.id == brain.id) {
            self.pages.remove(index).filter(|p| {
                if p.brain.route == brain.route {
                    true
                } else {
                    p.view.removeFromSuperview();
                    false
                }
            })
        } else {
            None
        };
        let selected = match selected {
            Some(page) => page,
            None => {
                let page = Page::new(id, brain, path.into(), app, app.mtm())?;
                self.root.addSubview(&page.view);
                page
            }
        };
        for page in &self.pages {
            page.view.setHidden(true);
        }
        selected.view.setHidden(false);
        self.native
            .setTitle(&NSString::from_str(&selected.brain.name));
        self.native.makeFirstResponder(Some(&selected.web));
        self.pages.push_back(selected.clone());
        while self.pages.len() > 4 {
            if let Some(old) = self.pages.pop_front() {
                old.view.removeFromSuperview();
            }
        }
        self.refresh(directory, show_tabs);
        Ok(selected)
    }
    pub fn refresh(&mut self, brains: &[Brain], show_tabs: bool) {
        let selected = self
            .pages
            .back()
            .map_or("", |p| p.brain.id.as_str())
            .to_owned();
        self.pages.retain(|p| {
            if p.brain.id == selected || brains.iter().any(|b| b.id == p.brain.id) {
                true
            } else {
                p.view.removeFromSuperview();
                false
            }
        });
        self.show_tabs = show_tabs;
        self.tabs.view.setHidden(!show_tabs);
        self.tabs.update(brains, &selected);
        self.layout();
    }
    pub fn fade(&self, reduce: bool) {
        super::app_delegate::fade_view(&self.tabs.view, reduce);
    }
    pub fn layout(&self) {
        let bounds = self.root.bounds();
        let bar = if self.show_tabs { HEIGHT } else { 0.0 };
        let page = NSRect::new(
            NSPoint::ZERO,
            NSSize::new(bounds.size.width, (bounds.size.height - bar).max(0.0)),
        );
        let inset = if self
            .native
            .styleMask()
            .contains(NSWindowStyleMask::FullScreen)
        {
            16.0
        } else {
            self.native
                .standardWindowButton(NSWindowButton::ZoomButton)
                .and_then(|b| {
                    // SAFETY: the retained button belongs to the live AppKit window.
                    Some(unsafe { b.superview() }?.convertRect_toView(b.frame(), None))
                })
                .map_or(88.0, |rect| rect.origin.x + rect.size.width + 20.0)
        };
        self.tabs.layout(
            NSRect::new(
                NSPoint::new(0.0, page.size.height),
                NSSize::new(bounds.size.width, bar),
            ),
            inset,
        );
        for tab in &self.pages {
            tab.layout(page, !self.show_tabs);
        }
    }
}

define_class!(
    // SAFETY: NSObject subclass; app is weak and UI methods run on the main thread.
    #[unsafe(super(NSObject))]
    #[thread_kind=objc2::MainThreadOnly]
    #[ivars=(Weak<AppDelegate>,u64)]
    pub(super) struct WindowHooks;
    // SAFETY: inherits the NSObject API.
    unsafe impl NSObjectProtocol for WindowHooks {}
    // SAFETY: NSWindowDelegate selectors are called with the specified NSNotification.
    unsafe impl NSWindowDelegate for WindowHooks {
        #[unsafe(method(windowWillClose:))]
        fn close(&self,_:&NSNotification){if let Some(app)=self.ivars().0.load(){app.close_window(self.ivars().1);}}
        #[unsafe(method(windowDidResize:))]
        fn resize(&self,_:&NSNotification){self.layout();}
        #[unsafe(method(windowDidEnterFullScreen:))]
        fn enter_full(&self,_:&NSNotification){self.layout();}
        #[unsafe(method(windowDidExitFullScreen:))]
        fn exit_full(&self,_:&NSNotification){self.layout();}
    }
);
impl WindowHooks {
    pub(super) fn select(&self, index: usize, detach: bool) {
        if let Some(app) = self.ivars().0.load() {
            app.select_tab(self.ivars().1, index, detach);
        }
    }
    fn layout(&self) {
        if let Some(app) = self.ivars().0.load() {
            app.layout_window(self.ivars().1);
        }
    }
}

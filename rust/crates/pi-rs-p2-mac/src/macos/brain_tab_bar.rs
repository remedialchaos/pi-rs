use super::{title_bar, window_controller::WindowHooks};
use crate::brain::{Brain, Status};
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Retained, Weak},
};
use objc2_app_kit::{
    NSBezierPath, NSButton, NSCellImagePosition, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSEvent, NSEventModifierFlags, NSFont, NSImage, NSView,
    NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use std::cell::Cell;

pub(super) const HEIGHT: f64 = 52.0;
pub(super) struct BrainTabBar {
    pub view: Retained<NSView>,
    buttons: Vec<Retained<Tab>>,
    brains: Vec<Brain>,
    hooks: Weak<WindowHooks>,
    mtm: MainThreadMarker,
}
impl BrainTabBar {
    pub fn new(hooks: &WindowHooks, mtm: MainThreadMarker) -> Self {
        // SAFETY: Bar inherits the NSView frame initializer with its declared ABI.
        let view: Retained<Bar> =
            unsafe { msg_send![super(Bar::alloc(mtm).set_ivars(())), initWithFrame: NSRect::ZERO] };
        Self {
            view: Retained::into_super(view),
            buttons: Vec::new(),
            brains: Vec::new(),
            hooks: Weak::new(hooks),
            mtm,
        }
    }
    pub fn update(&mut self, brains: &[Brain], selected: &str) {
        if self.brains != brains
            && let Some(target) = self.hooks.load()
        {
            let first = brains.iter().find_map(|brain| brain.organization.as_ref());
            let grouped = first.is_some_and(|org| {
                brains
                    .iter()
                    .any(|brain| brain.organization.as_ref().is_some_and(|b| b.id != org.id))
            });
            for b in self.buttons.drain(..) {
                b.removeFromSuperview();
            }
            for (i, brain) in brains.iter().enumerate() {
                let mut title = String::new();
                title.push_str(match brain.status {
                    Status::Online => "● ",
                    Status::Offline => "○ ",
                    Status::Unknown => "◌ ",
                });
                if grouped && let Some(org) = &brain.organization {
                    title.push_str(&org.name);
                    title.push_str(" / ");
                }
                title.push_str(&brain.label());
                let state = (Weak::new(&*target), i, Cell::new(false));
                // SAFETY: Tab inherits NSButton's NSView frame initializer and returns a retained object.
                let button: Retained<Tab> = unsafe {
                    msg_send![super(Tab::alloc(self.mtm).set_ivars(state)), initWithFrame: NSRect::ZERO]
                };
                button.setTitle(&NSString::from_str(&title));
                if brain.is_direct() {
                    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                        &NSString::from_str("laptopcomputer"),
                        Some(&NSString::from_str("On this Mac")),
                    );
                    button.setImage(image.as_deref());
                    button.setImagePosition(NSCellImagePosition::ImageTrailing);
                }
                button.setFont(Some(&NSFont::systemFontOfSize(14.0)));
                button.setBordered(false);
                let color = if brain.away() {
                    NSColor::secondaryLabelColor()
                } else {
                    NSColor::labelColor()
                };
                button.setContentTintColor(Some(&color));
                button.setToolTip(Some(&NSString::from_str(
                    &brain.organization.as_ref().map_or_else(
                        || "This Mac".into(),
                        |org| format!("{} · {}", org.name, brain.id),
                    ),
                )));
                self.view.addSubview(&button);
                self.buttons.push(button);
            }
            self.brains = brains.to_vec();
        }
        for (brain, button) in self.brains.iter().zip(&self.buttons) {
            button.setState(if brain.id == selected {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            button.highlight(brain.id == selected);
        }
    }
    pub fn brain(&self, index: usize) -> Option<&str> {
        self.brains.get(index).map(|brain| brain.id.as_str())
    }
    pub fn layout(&self, frame: NSRect, inset: f64) {
        self.view.setFrame(frame);
        let mut x = inset;
        for button in &self.buttons {
            let width = button.intrinsicContentSize().width.max(36.0);
            button.setFrame(NSRect::new(
                NSPoint::new(x, 2.0),
                NSSize::new(width, HEIGHT - 4.0),
            ));
            x += width + 16.0;
        }
    }
}

define_class!(
    // SAFETY: NSView subclass; AppKit sends mouse and drawing callbacks on the main thread.
    #[unsafe(super(NSView))]
    #[thread_kind=objc2::MainThreadOnly]
    struct Bar;
    impl Bar {
        #[unsafe(method(mouseDown:))]
        fn down(&self, event: &NSEvent) {
            if let Some(window) = self.window()
                && !window.styleMask().contains(NSWindowStyleMask::FullScreen) {
                title_bar::drag(&window, event);
            }
        }
        #[unsafe(method(drawRect:))]
        fn draw(&self, _dirty: NSRect) {
            let bounds = self.bounds();
            NSColor::windowBackgroundColor().setFill();
            NSBezierPath::fillRect(bounds);
            NSColor::separatorColor().setFill();
            NSBezierPath::fillRect(NSRect::new(bounds.origin, NSSize::new(bounds.size.width, 2.0)));
        }
    }
);

define_class!(
    // SAFETY: NSButton subclass; ivars own a weak reference and mouse callbacks stay on the main thread.
    #[unsafe(super(NSButton, objc2_app_kit::NSControl, NSView, objc2_app_kit::NSResponder, objc2_foundation::NSObject))]
    #[thread_kind=objc2::MainThreadOnly]
    #[ivars=(Weak<WindowHooks>, usize, Cell<bool>)]
    struct Tab;
    impl Tab {
        #[unsafe(method(mouseDown:))]
        fn down(&self, _event: &NSEvent) {
            self.ivars().2.set(false);
        }
        #[unsafe(method(mouseDragged:))]
        fn dragged(&self, event: &NSEvent) {
            if self.ivars().2.get() { return; }
            // SAFETY: this AppKit callback borrows the tab and obtains a retained parent view if present.
            if let Some(bar) = unsafe { self.superview() } {
                let point = bar.convertPoint_fromView(event.locationInWindow(), None);
                if point.y < -24.0 || point.y > bar.bounds().size.height + 24.0 {
                    self.ivars().2.set(true);
                    self.activate(true);
                }
            }
        }
        #[unsafe(method(mouseUp:))]
        fn up(&self, event: &NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let bounds = self.bounds();
            if !self.ivars().2.get() && self.mouse_inRect(point, bounds) {
                self.activate(event.modifierFlags().contains(NSEventModifierFlags::Command));
            }
        }
        #[unsafe(method(otherMouseUp:))]
        fn other(&self, event: &NSEvent) {
            if event.buttonNumber() == 2 { self.activate(true); }
        }
        #[unsafe(method(accessibilityPerformPress))]
        fn accessible_press(&self) -> bool {
            self.activate(false);
            true
        }
    }
);
impl Tab {
    fn activate(&self, detach: bool) {
        if let Some(hooks) = self.ivars().0.load() {
            hooks.select(self.ivars().1, detach);
        }
    }
}

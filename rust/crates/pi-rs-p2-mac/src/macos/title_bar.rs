use objc2_app_kit::{NSEvent, NSWindow};
use objc2_foundation::{NSString, NSUserDefaults};

pub(super) fn drag(window: &NSWindow, event: &NSEvent) {
    if event.clickCount() != 2 {
        window.performWindowDragWithEvent(event);
        return;
    }
    // AppKit has no public API for the "Fill" preference; the zoom action is the documented fallback.
    let action = NSUserDefaults::standardUserDefaults()
        .stringForKey(&NSString::from_str("AppleActionOnDoubleClick"))
        .map(|s| s.to_string());
    match action.as_deref() {
        Some("Minimize") => window.performMiniaturize(None),
        Some("None") => {}
        _ => window.performZoom(None),
    }
}

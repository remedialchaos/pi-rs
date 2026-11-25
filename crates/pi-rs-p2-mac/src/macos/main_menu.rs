use crate::appearance::Appearance;
use objc2::{
    MainThreadMarker, MainThreadOnly,
    rc::Retained,
    runtime::{AnyObject, Sel},
    sel,
};
use objc2_app_kit::{NSApplication, NSEventModifierFlags as Mod, NSMenu, NSMenuItem};
use objc2_foundation::NSString;

pub(super) fn build(app: &NSApplication, target: &AnyObject, mtm: MainThreadMarker) {
    let root = NSMenu::new(mtm);
    let pi = submenu(&root, "Pi", mtm);
    pi.addItem(&item(
        "About Pi",
        Some(sel!(orderFrontStandardAboutPanel:)),
        "",
        None,
        mtm,
    ));
    pi.addItem(&item(
        "Check for Updates…",
        Some(sel!(checkUpdates:)),
        "",
        Some(target),
        mtm,
    ));
    pi.addItem(&NSMenuItem::separatorItem(mtm));
    pi.addItem(&item(
        "Settings…",
        Some(sel!(openSettings:)),
        ",",
        Some(target),
        mtm,
    ));
    pi.addItem(&NSMenuItem::separatorItem(mtm));
    pi.addItem(&item("Hide Pi", Some(sel!(hide:)), "h", None, mtm));
    let hide_others = item(
        "Hide Others",
        Some(sel!(hideOtherApplications:)),
        "h",
        None,
        mtm,
    );
    hide_others.setKeyEquivalentModifierMask(Mod::Command | Mod::Option);
    pi.addItem(&hide_others);
    pi.addItem(&item(
        "Show All",
        Some(sel!(unhideAllApplications:)),
        "",
        None,
        mtm,
    ));
    pi.addItem(&NSMenuItem::separatorItem(mtm));
    pi.addItem(&item(
        "Quit Pi",
        Some(sel!(terminate:)),
        "q",
        Some(app),
        mtm,
    ));
    let file = submenu(&root, "File", mtm);
    let new = item("New Window", Some(sel!(newWindow:)), "n", Some(target), mtm);
    new.setKeyEquivalentModifierMask(Mod::Command | Mod::Shift);
    file.addItem(&new);
    file.addItem(&NSMenuItem::separatorItem(mtm));
    file.addItem(&item(
        "Close Window",
        Some(sel!(performClose:)),
        "w",
        None,
        mtm,
    ));
    let edit = submenu(&root, "Edit", mtm);
    for (title, action, key, modifiers) in [
        ("Undo", sel!(undo:), "z", Mod::Command),
        ("Redo", sel!(redo:), "z", Mod::Command | Mod::Shift),
        ("Cut", sel!(cut:), "x", Mod::Command),
        ("Copy", sel!(copy:), "c", Mod::Command),
        ("Paste", sel!(paste:), "v", Mod::Command),
        (
            "Paste and Match Style",
            sel!(pasteAsPlainText:),
            "v",
            Mod::Command | Mod::Option | Mod::Shift,
        ),
        ("Select All", sel!(selectAll:), "a", Mod::Command),
    ] {
        if title == "Cut" {
            edit.addItem(&NSMenuItem::separatorItem(mtm));
        }
        let entry = item(title, Some(action), key, None, mtm);
        entry.setKeyEquivalentModifierMask(modifiers);
        edit.addItem(&entry);
    }
    let view = submenu(&root, "View", mtm);
    for (title, action, key) in [
        ("Reload Page", sel!(reloadPage:), "r"),
        ("Actual Size", sel!(setZoom:), "0"),
        ("Zoom In", sel!(setZoom:), "+"),
        ("Zoom Out", sel!(setZoom:), "-"),
    ] {
        let entry = item(title, Some(action), key, Some(target), mtm);
        entry.setTag(match key {
            "+" => 1,
            "-" => -1,
            _ => 0,
        });
        view.addItem(&entry);
        if key == "r" {
            view.addItem(&NSMenuItem::separatorItem(mtm));
        }
    }
    view.addItem(&NSMenuItem::separatorItem(mtm));
    let appearance = submenu(&view, "Appearance", mtm);
    for (i, preference) in Appearance::ALL.into_iter().enumerate() {
        let entry = item(
            preference.title(),
            Some(sel!(appearance:)),
            "",
            Some(target),
            mtm,
        );
        entry.setTag(i as isize);
        appearance.addItem(&entry);
    }
    view.addItem(&NSMenuItem::separatorItem(mtm));
    let full = item(
        "Enter Full Screen",
        Some(sel!(toggleFullScreen:)),
        "f",
        None,
        mtm,
    );
    full.setKeyEquivalentModifierMask(Mod::Command | Mod::Control);
    view.addItem(&full);
    let windows = submenu(&root, "Window", mtm);
    windows.addItem(&item(
        "Minimize",
        Some(sel!(performMiniaturize:)),
        "m",
        None,
        mtm,
    ));
    windows.addItem(&item("Zoom", Some(sel!(performZoom:)), "", None, mtm));
    windows.addItem(&NSMenuItem::separatorItem(mtm));
    windows.addItem(&item(
        "Bring All to Front",
        Some(sel!(arrangeInFront:)),
        "",
        None,
        mtm,
    ));
    app.setWindowsMenu(Some(&windows));
    app.setMainMenu(Some(&root));
}

pub(super) fn submenu(parent: &NSMenu, title: &str, mtm: MainThreadMarker) -> Retained<NSMenu> {
    let child = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
    let holder = item(title, None, "", None, mtm);
    holder.setSubmenu(Some(&child));
    parent.addItem(&holder);
    child
}
pub(super) fn item(
    title: &str,
    action: Option<Sel>,
    key: &str,
    target: Option<&AnyObject>,
    mtm: MainThreadMarker,
) -> Retained<NSMenuItem> {
    // SAFETY: all selectors are standard responder actions or AppDelegate's one-argument methods. The app retains targets.
    unsafe {
        let item = NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            action,
            &NSString::from_str(key),
        );
        item.setTarget(target);
        item
    }
}

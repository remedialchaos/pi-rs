//! AppKit objects live on the main thread; only portable values cross to Tokio.
#![expect(
    unsafe_code,
    reason = "AppKit and WebKit delegate and generated Objective-C ABI calls"
)]
mod app_delegate;
mod brain_page;
mod brain_tab_bar;
mod main_menu;
mod title_bar;
mod window_chrome;
mod window_controller;

use crate::{Error, Result, server_config::Preferences};
use objc2::MainThreadMarker;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSWorkspace};
use objc2_foundation::{NSString, NSURL, NSUserDefaults};

pub fn run() -> Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| Error::Protocol("AppKit must start on the main thread".into()))?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let delegate = app_delegate::AppDelegate::new(mtm)?;
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
    Ok(())
}

struct MacPreferences;
impl Preferences for MacPreferences {
    fn get(&self, key: &str) -> Option<String> {
        NSUserDefaults::standardUserDefaults()
            .stringForKey(&NSString::from_str(key))
            .map(|s| s.to_string())
    }
    fn set(&mut self, key: &str, value: Option<&str>) -> Result<()> {
        let defaults = NSUserDefaults::standardUserDefaults();
        let key = NSString::from_str(key);
        // SAFETY: NSString is a property-list object supported by NSUserDefaults.
        if let Some(value) = value {
            unsafe { defaults.setObject_forKey(Some(&NSString::from_str(value)), &key) };
        } else {
            defaults.removeObjectForKey(&key);
        }
        Ok(())
    }
}

fn open_url(url: &url::Url) -> Result<()> {
    let native = NSURL::URLWithString(&NSString::from_str(url.as_str()))
        .ok_or_else(|| Error::Protocol("could not open URL".into()))?;
    if NSWorkspace::sharedWorkspace().openURL(&native) {
        Ok(())
    } else {
        Err(Error::Protocol("could not open URL".into()))
    }
}

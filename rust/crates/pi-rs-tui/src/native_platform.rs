//! Platform clipboard reads and keyboard integration.

use std::fmt;

#[cfg(target_os = "linux")]
mod linux;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifierKey {
    Shift,
    Command,
    Control,
    Option,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ClipboardRead<T> {
    Unavailable,
    Empty,
    Data(T),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardError;

impl fmt::Display for ClipboardError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("Could not read X11 clipboard")
    }
}

impl std::error::Error for ClipboardError {}

#[derive(Debug, Clone, Copy)]
pub struct NativeClipboard(());

pub fn get_native_clipboard() -> Option<NativeClipboard> {
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_some_and(|display| !display.is_empty()) {
        return Some(NativeClipboard(()));
    }
    None
}

impl NativeClipboard {
    pub fn get_text(self) -> Result<ClipboardRead<String>, ClipboardError> {
        #[cfg(target_os = "linux")]
        return linux::read(false).map(|result| match result {
            ClipboardRead::Unavailable => ClipboardRead::Unavailable,
            ClipboardRead::Empty => ClipboardRead::Empty,
            ClipboardRead::Data((bytes, true)) => {
                ClipboardRead::Data(bytes.into_iter().map(char::from).collect())
            }
            ClipboardRead::Data((bytes, false)) => ClipboardRead::Data(
                String::from_utf8(bytes)
                    .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()),
            ),
        });
        #[cfg(not(target_os = "linux"))]
        Ok(ClipboardRead::Unavailable)
    }

    pub fn get_image(self) -> Result<ClipboardRead<Vec<u8>>, ClipboardError> {
        #[cfg(target_os = "linux")]
        return linux::read(true).map(|result| match result {
            ClipboardRead::Unavailable => ClipboardRead::Unavailable,
            ClipboardRead::Empty => ClipboardRead::Empty,
            ClipboardRead::Data((bytes, _)) => ClipboardRead::Data(bytes),
        });
        #[cfg(not(target_os = "linux"))]
        Ok(ClipboardRead::Unavailable)
    }
}

pub fn is_native_modifier_pressed(_modifier: ModifierKey) -> bool {
    false
}

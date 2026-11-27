//! Platform clipboard reads and keyboard integration.

use std::fmt;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

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
        #[cfg(target_os = "linux")]
        out.write_str("Could not read X11 clipboard")?;
        #[cfg(target_os = "macos")]
        out.write_str("Could not read the macOS clipboard")?;
        #[cfg(windows)]
        out.write_str("Could not read the Windows clipboard")?;
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        out.write_str("Could not read the clipboard")?;
        Ok(())
    }
}

impl std::error::Error for ClipboardError {}

#[derive(Debug, Clone, Copy)]
pub struct NativeClipboard(());

/// The platform clipboard, if the platform has one.
pub fn get_native_clipboard() -> Option<NativeClipboard> {
    #[cfg(target_os = "linux")]
    return std::env::var_os("DISPLAY")
        .is_some_and(|display| !display.is_empty())
        .then_some(NativeClipboard(()));
    #[cfg(target_os = "macos")]
    return Some(NativeClipboard(()));
    #[cfg(windows)]
    return Some(NativeClipboard(()));
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    None
}

impl NativeClipboard {
    /// The clipboard text, empty when the clipboard holds none.
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
        #[cfg(target_os = "macos")]
        return macos::get_text();
        #[cfg(windows)]
        return windows::get_text();
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        Ok(ClipboardRead::Unavailable)
    }

    /// The clipboard image bytes, empty when the clipboard holds no image.
    pub fn get_image(self) -> Result<ClipboardRead<Vec<u8>>, ClipboardError> {
        #[cfg(target_os = "linux")]
        return linux::read(true).map(|result| match result {
            ClipboardRead::Unavailable => ClipboardRead::Unavailable,
            ClipboardRead::Empty => ClipboardRead::Empty,
            ClipboardRead::Data((bytes, _)) => ClipboardRead::Data(bytes),
        });
        #[cfg(target_os = "macos")]
        return macos::get_image();
        #[cfg(windows)]
        return windows::get_image();
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        Ok(ClipboardRead::Unavailable)
    }

    /// The POSIX paths of the files on the clipboard, empty when it holds none.
    pub fn get_file_paths(self) -> Result<ClipboardRead<Vec<String>>, ClipboardError> {
        #[cfg(target_os = "linux")]
        return Ok(ClipboardRead::Unavailable);
        #[cfg(target_os = "macos")]
        return macos::get_file_paths();
        #[cfg(windows)]
        return windows::get_file_paths();
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        Ok(ClipboardRead::Unavailable)
    }

    /// Writes text to the clipboard.
    pub fn set_text(self, text: &str) -> Result<(), ClipboardError> {
        #[cfg(target_os = "linux")]
        return {
            // Linux keeps the clipboard with command-line tools, so the app does not write it.
            let _ = text;
            Err(ClipboardError)
        };
        #[cfg(target_os = "macos")]
        return macos::set_text(text);
        #[cfg(windows)]
        return windows::set_text(text);
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        Err(ClipboardError)
    }
}

/// Whether the platform reports the modifier key as pressed right now.
pub fn is_native_modifier_pressed(modifier: ModifierKey) -> bool {
    #[cfg(target_os = "linux")]
    return {
        let _ = modifier;
        false
    };
    #[cfg(target_os = "macos")]
    return macos::is_modifier_pressed(modifier);
    #[cfg(windows)]
    return windows::is_modifier_pressed(modifier);
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = modifier;
        false
    }
}
